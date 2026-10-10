//! The scheduled tasks page through the mounted UI and the real native server: the live list, a
//! task of each kind made from the form, the form's refusals, editing, pausing, running now
//! through the provider fixture, a failed run's reason, deleting after a confirmation, a
//! webhook's address, rotation and delivery log, a connection that cannot change anything and
//! one that was lost. Waits are on text or controls that only the awaited step produces.
use super::diff_tests::{Fixture, Harness, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::{PALETTE, key_on, open_palette};
use crate::runtime::transport_tests::{change_control, change_when_enabled, click_control, click_when_enabled, control, control_attribute, input_control};

const OPERATE: [AuthEnvironmentScope; 2] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate];
const READ: [AuthEnvironmentScope; 1] = [AuthEnvironmentScope::OrchestrationRead];
const FORM: &str = "Task editor";

async fn open(fixture: &Fixture, scopes: &[AuthEnvironmentScope]) -> (VirtualDom, Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.client(Gate::default(), scopes);
    let state = if scopes.contains(&AuthEnvironmentScope::OrchestrationOperate) {
        seed(&mut dom, &props, &fixture.workspace).await
    } else {
        let state = props.state.borrow().unwrap();
        until(&mut dom, "the shell", |_| state.peek().shell.synchronized).await;
        state
    };
    state.view().set(View::ScheduledTasks);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    (dom, props, state)
}

fn value(dom: &VirtualDom, label: &str) -> Option<String> {
    control_attribute(dom, label, "value").and_then(|(_, value)| value)
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}

/// Opens the form on a new task and waits for the first model the page resolves.
async fn begin(dom: &mut VirtualDom) {
    click_control(dom, "New task");
    until(dom, "the form", |dom| control(dom, "Name").is_some()).await;
    until(dom, "a default model", |dom| value(dom, "Model").is_some_and(|model| model.starts_with('['))).await;
}
/// A task on the project's own checkout, as the form's current server gaps allow runs.
fn fill(dom: &mut VirtualDom, name: &str, prompt: &str) {
    input_control(dom, "Name", name);
    input_control(dom, "Prompt", prompt);
    change_control(dom, "Workspace", "root");
}
async fn create(dom: &mut VirtualDom) {
    click_control(dom, "Create task");
    until(dom, "the form to close", |dom| control(dom, FORM).is_none()).await;
}
fn upsert(dom: &mut VirtualDom, props: &Harness, state: Store<UiModel>, overrides: Value) {
    let mut input = json!({"title":"Review","prompt":"Review the open pull requests.","enabled":true,"schedule":{"type":"interval","everyMs":300_000},"projectId":"project","workspaceStrategy":{"type":"root"},
        "modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default"});
    for (key, value) in overrides.as_object().unwrap() {
        input[key] = value.clone();
    }
    dom.in_scope(ScopeId::APP, || request(&props.transport, state, "scheduledTasks.upsert", input, RequestKind::Unary)).expect("the upsert is sent");
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_of_each_kind_is_made_from_the_form_and_listed_with_its_schedule() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, _state) = open(&fixture, &OPERATE).await;
    wait_for_rendered_text(&mut dom, "No scheduled tasks").await;
    assert!(rendered_text(&dom).contains("which may differ from yours"), "the page says whose clock a time of day follows");
    assert!(!disabled(&dom, "New task"));

    // At a time: the default schedule is weekdays at nine; a day toggled on is listed.
    begin(&mut dom).await;
    fill(&mut dom, "Morning review", "Review the open pull requests.");
    input_control(&mut dom, "Run at", "07:30");
    click_control(&mut dom, "Sat");
    assert_eq!(control_attribute(&dom, "Sat", "aria-pressed").unwrap().1.as_deref(), Some("true"));
    create(&mut dom).await;
    wait_for_rendered_text(&mut dom, "Mon, Tue, Wed, Thu, Fri, Sat at 07:30 \u{b7} Next run").await;

    // Every interval.
    begin(&mut dom).await;
    fill(&mut dom, "Interval review", "Look again.");
    click_control(&mut dom, "Every interval");
    input_control(&mut dom, "Run every", "5");
    create(&mut dom).await;
    wait_for_rendered_text(&mut dom, "Every 5 min \u{b7} Next run").await;

    // On a webhook: a blank prompt starts as the webhook prompt, and the address waits for the save.
    begin(&mut dom).await;
    input_control(&mut dom, "Name", "Signed hook");
    change_control(&mut dom, "Workspace", "root");
    click_control(&mut dom, "On webhook");
    assert_eq!(value(&dom, "Prompt").as_deref(), Some("Handle this webhook:\n{{body}}"));
    assert!(rendered_text(&dom).contains("The URL appears after you save."));
    change_control(&mut dom, "Require signature", "true");
    input_control(&mut dom, "Secret", "s3cret");
    input_control(&mut dom, "Skip requests older than", "30");
    create(&mut dom).await;
    wait_for_rendered_text(&mut dom, "On webhook \u{b7} Listening").await;
    assert!(!rendered_text(&dom).contains("s3cret"), "a secret is never shown");
    let text = rendered_text(&dom);
    assert_eq!(text.matches("Next run").count(), 2, "webhook tasks have no next run: {text}");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_form_says_why_it_cannot_save_and_every_dialog_closes_with_escape() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &OPERATE).await;
    wait_for_rendered_text(&mut dom, "No scheduled tasks").await;
    begin(&mut dom).await;
    click_control(&mut dom, "Create task");
    assert!(rendered_text(&dom).contains("Scheduled task is incomplete. Add a title, prompt, project, and model."));
    fill(&mut dom, "Checks", "Run the checks.");
    click_control(&mut dom, "Every interval");
    for bad in ["0.5", "", "soon"] {
        input_control(&mut dom, "Run every", bad);
        click_control(&mut dom, "Create task");
        assert!(rendered_text(&dom).contains("Invalid interval. Enter an interval of at least one minute."), "{bad:?}");
    }
    click_control(&mut dom, "At a time");
    input_control(&mut dom, "Run at", "25:00");
    click_control(&mut dom, "Create task");
    assert!(rendered_text(&dom).contains("Invalid time. Enter a time of day such as 09:30."));
    click_control(&mut dom, "On webhook");
    change_control(&mut dom, "Require signature", "true");
    click_control(&mut dom, "Create task");
    assert!(rendered_text(&dom).contains("Signing secret is required. Enter the signature header and secret."));
    input_control(&mut dom, "Secret", "s");
    input_control(&mut dom, "Skip requests older than", "1441");
    click_control(&mut dom, "Create task");
    assert!(rendered_text(&dom).contains("Invalid age limit. Enter whole minutes from 1 to 1440, or leave it blank."));
    change_control(&mut dom, "Workspace", "existing_worktree");
    input_control(&mut dom, "Skip requests older than", "");
    click_control(&mut dom, "Create task");
    assert!(rendered_text(&dom).contains("Checkout path is required. Enter the path of the checkout to run in."));
    // Nothing was sent, and Escape leaves the form.
    key_on(&mut dom, FORM, Key::Escape);
    assert!(control(&dom, FORM).is_none());
    assert!(rendered_text(&dom).contains("No scheduled tasks"));
    assert!(state.peek().error.is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_task_is_edited_paused_run_now_and_deleted_after_a_confirmation() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = open(&fixture, &OPERATE).await;
    // The form saves only against a list that has loaded.
    wait_for_rendered_text(&mut dom, "No scheduled tasks").await;
    begin(&mut dom).await;
    fill(&mut dom, "Interval review", "Review the open pull requests.");
    click_control(&mut dom, "Every interval");
    input_control(&mut dom, "Run every", "5");
    create(&mut dom).await;
    wait_for_rendered_text(&mut dom, "Every 5 min \u{b7} Next run").await;

    // Edit: the form opens on the task, and a saved change shows in the list.
    click_control(&mut dom, "Edit Interval review");
    until(&mut dom, "the form", |dom| control(dom, FORM).is_some()).await;
    assert_eq!((value(&dom, "Name").as_deref(), value(&dom, "Run every").as_deref()), (Some("Interval review"), Some("5")));
    input_control(&mut dom, "Run every", "10");
    click_control(&mut dom, "Save task");
    until(&mut dom, "the form to close", |dom| control(dom, FORM).is_none()).await;
    wait_for_rendered_text(&mut dom, "Every 10 min \u{b7} Next run").await;
    // Cancel leaves the task alone.
    click_control(&mut dom, "Edit Interval review");
    input_control(&mut dom, "Run every", "30");
    click_control(&mut dom, "Cancel");
    assert!(control(&dom, FORM).is_none());
    assert!(rendered_text(&dom).contains("Every 10 min"));

    // Pause and resume.
    change_when_enabled(&mut dom, "Enable Interval review", "false").await;
    wait_for_rendered_text(&mut dom, "Every 10 min \u{b7} Paused").await;
    change_when_enabled(&mut dom, "Enable Interval review", "true").await;
    wait_for_rendered_text(&mut dom, "Every 10 min \u{b7} Next run").await;

    // Run now reaches the provider: a thread of the task's title is made, and the run is recorded.
    click_when_enabled(&mut dom, "Run Interval review now").await;
    wait_for_rendered_text(&mut dom, "succeeded").await;
    until(&mut dom, "the task's thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.title.as_str() == "Interval review"))).await;
    assert!(rendered_text(&dom).contains("Last run"));

    // A run that fails says why, plainly.
    upsert(&mut dom, &props, state, json!({"id":"scheduled-task:broken","title":"Broken","threadId":"no-such-thread"}));
    wait_for_rendered_text(&mut dom, "Broken").await;
    click_control(&mut dom, "Run Broken now");
    wait_for_rendered_text(&mut dom, "failed").await;
    assert!(control(&dom, "Last run error of Broken").is_some());
    assert!(rendered_text(&dom).contains("no-such-thread"));

    // Delete asks first; Escape declines and Delete task goes ahead.
    let question = "Delete \u{201c}Interval review\u{201d}?";
    click_control(&mut dom, "Delete Interval review");
    assert!(control(&dom, question).is_some());
    key_on(&mut dom, question, Key::Escape);
    assert!(control(&dom, question).is_none());
    assert!(control(&dom, "Edit Interval review").is_some(), "declining deletes nothing");
    click_control(&mut dom, "Delete Interval review");
    click_control(&mut dom, "Cancel delete");
    assert!(control(&dom, question).is_none());
    click_control(&mut dom, "Delete Interval review");
    click_control(&mut dom, "Delete task");
    until(&mut dom, "the task to go", |dom| control(dom, "Edit Interval review").is_none()).await;
    assert!(control(&dom, "Edit Broken").is_some());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_webhook_task_shows_its_address_rotates_after_a_confirmation_and_keeps_a_delivery_log() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = open(&fixture, &OPERATE).await;
    upsert(&mut dom, &props, state, json!({"id":"scheduled-task:open","title":"Open hook","prompt":"Got {{body}}","schedule":{"type":"webhook","signature":null}}));
    upsert(&mut dom, &props, state, json!({"id":"scheduled-task:signed","title":"Signed hook","schedule":{"type":"webhook","signature":{"header":"X-Sig","encoding":"hex","prefix":"sha256=","secret":"s3cret"}}}));
    wait_for_rendered_text(&mut dom, "Signed hook").await;
    wait_for_rendered_text(&mut dom, "Open hook").await;
    let url = |dom: &VirtualDom| value(dom, "Webhook URL").unwrap_or_default();

    click_control(&mut dom, "Edit Signed hook");
    until(&mut dom, "the address", |dom| url(dom).contains("/api/hooks/")).await;
    let first = url(&dom);
    assert!(first.starts_with("http://127.0.0.1:"), "the address this client reaches the environment at: {first}");
    assert!(rendered_text(&dom).contains("Only this computer can call this address."));
    assert_eq!(control_attribute(&dom, "Require signature", "checked").unwrap().1.as_deref(), Some("true"));
    assert_eq!(control_attribute(&dom, "Secret", "placeholder").unwrap().1.as_deref(), Some("Unchanged"), "a stored secret is kept unless a new one is typed");
    assert!(!rendered_text(&dom).contains("s3cret") && !first.contains("s3cret"), "the stored secret is not shown");
    assert!(!disabled(&dom, "Copy webhook URL"));

    // Rotation asks first.
    let question = "Rotate this webhook URL?";
    click_control(&mut dom, "Rotate webhook URL");
    assert!(control(&dom, question).is_some() && rendered_text(&dom).contains("The current URL stops working."));
    key_on(&mut dom, question, Key::Escape);
    assert!(control(&dom, question).is_none() && control(&dom, FORM).is_some(), "Escape closes only the question");
    assert_eq!(url(&dom), first);
    click_control(&mut dom, "Rotate webhook URL");
    click_control(&mut dom, "Rotate");
    until(&mut dom, "a new address", |dom| url(dom) != first && url(dom).contains("/api/hooks/")).await;
    let rotated = url(&dom);
    let client = reqwest::Client::new();
    assert_eq!(client.post(&first).body("{}").send().await.unwrap().status(), 404, "the old address stops working");
    // A request the signature refuses is logged, without starting a run.
    assert_eq!(client.post(&rotated).body("{}").send().await.unwrap().status(), 401);
    key_on(&mut dom, FORM, Key::Escape);

    click_control(&mut dom, "Edit Open hook");
    until(&mut dom, "the address", |dom| url(dom).contains("/api/hooks/")).await;
    let open_url = url(&dom);
    key_on(&mut dom, FORM, Key::Escape);
    let accepted = client.post(&open_url).header("authorization", "Bearer abc").body(r#"{"hello":"world"}"#).send().await.unwrap();
    assert_eq!(accepted.status(), 202);
    let delivery = accepted.json::<Value>().await.unwrap()["deliveryId"].as_str().unwrap().to_owned();

    // The log lists each request, and a delivery opens with what it sent.
    click_control(&mut dom, "Deliveries of Open hook");
    let log = "Delivery log of Open hook";
    until(&mut dom, "the log", |dom| control(dom, &format!("Open delivery {delivery}")).is_some()).await;
    assert!(rendered_text(&dom).contains("Ran") && rendered_text(&dom).contains("POST"));
    click_control(&mut dom, &format!("Open delivery {delivery}"));
    wait_for_rendered_text(&mut dom, "Prompt sent to the agent").await;
    let text = rendered_text(&dom);
    assert!(text.contains(r#"Got {"hello":"world"}"#) && text.contains(r#"{"hello":"world"}"#), "{text}");
    assert!(text.contains("authorization: [redacted]") && !text.contains("Bearer abc"), "credentials are never shown: {text}");
    click_control(&mut dom, "Back to deliveries");
    assert!(control(&dom, &format!("Open delivery {delivery}")).is_some());
    click_control(&mut dom, "Refresh deliveries");
    until(&mut dom, "the refreshed log", |dom| control(dom, &format!("Open delivery {delivery}")).is_some()).await;
    key_on(&mut dom, log, Key::Escape);
    assert!(control(&dom, log).is_none());
    click_control(&mut dom, "Deliveries of Signed hook");
    wait_for_rendered_text(&mut dom, "Bad signature").await;
    click_control(&mut dom, "Done");
    assert!(control(&dom, "Delivery log of Signed hook").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_cannot_operate_sees_the_tasks_but_no_controls_and_no_webhook_details() {
    let fixture = Fixture::start(false).await;
    let (mut operator, props, state) = open(&fixture, &OPERATE).await;
    upsert(&mut operator, &props, state, json!({"id":"scheduled-task:review","title":"Reader review"}));
    upsert(&mut operator, &props, state, json!({"id":"scheduled-task:hook","title":"Reader hook","schedule":{"type":"webhook","signature":null}}));
    wait_for_rendered_text(&mut operator, "Reader hook").await;
    wait_for_rendered_text(&mut operator, "Reader review").await;
    assert!(control(&operator, "Edit Reader hook").is_some());

    let (mut dom, _reader_props, _reader) = open(&fixture, &READ).await;
    wait_for_rendered_text(&mut dom, "Reader hook").await;
    wait_for_rendered_text(&mut dom, "Reader review").await;
    let text = rendered_text(&dom);
    assert!(text.contains("This connection can view scheduled tasks but cannot change them."));
    assert!(text.contains("On webhook \u{b7} Listening") && text.contains("Every 5 min"));
    for label in ["New task", "Edit Reader review", "Edit Reader hook", "Enable Reader review", "Run Reader review now", "Delete Reader review", "Deliveries of Reader hook"] {
        assert!(control(&dom, label).is_none(), "{label} must not be offered to a connection that cannot operate");
    }
    assert!(!text.contains("/api/hooks/"), "a webhook's address is not for this connection");
    drop(operator);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_lost_connection_marks_the_list_out_of_date_and_stops_every_change() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props, state) = open(&fixture, &OPERATE).await;
    upsert(&mut dom, &props, state, json!({"id":"scheduled-task:review","title":"Offline review"}));
    wait_for_rendered_text(&mut dom, "Offline review").await;
    assert!(!rendered_text(&dom).contains("may be out of date"));
    assert!(!disabled(&dom, "Edit Offline review") && !disabled(&dom, "New task"));
    // The connection drops; the last list stays, marked, and nothing can be changed from it.
    dom.in_scope(ScopeId::APP, || interrupt_connection(&props.transport, state, "the network went away"));
    wait_for_rendered_text(&mut dom, "This list may be out of date.").await;
    assert!(rendered_text(&dom).contains("Offline review"));
    for label in ["Edit Offline review", "Enable Offline review", "Run Offline review now", "Delete Offline review", "New task"] {
        assert!(disabled(&dom, label), "{label} must be disabled");
    }
    assert!(state.peek().error.is_none(), "nothing was sent that could fail");
    fixture.finish(dom).await;
}

#[derive(Clone)]
struct ProbeHarness {
    inner: Harness,
    probe: crate::RenderProbe,
}
fn probe_harness(props: ProbeHarness) -> Element {
    use_context_provider(|| props.probe.clone());
    super::diff_tests::harness(props.inner.clone())
}

#[tokio::test(flavor = "current_thread")]
async fn live_task_updates_and_the_form_never_repaint_the_sidebar_or_the_conversation() {
    let fixture = Fixture::start(false).await;
    let (_unused, inner) = fixture.client(Gate::default(), &OPERATE);
    let props = ProbeHarness { inner, probe: crate::RenderProbe::default() };
    let mut dom = VirtualDom::new_with_props(probe_harness, props.clone());
    dom.rebuild_in_place();
    let state = seed(&mut dom, &props.inner, &fixture.workspace).await;
    state.view().set(View::ScheduledTasks);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "No scheduled tasks").await;
    let (application, timeline, composer) = (props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get());
    for (index, title) in ["First", "Second", "Third"].into_iter().enumerate() {
        upsert(&mut dom, &props.inner, state, json!({"id":format!("scheduled-task:{index}"),"title":title}));
        wait_for_rendered_text(&mut dom, title).await;
    }
    change_control(&mut dom, "Enable Second", "false");
    wait_for_rendered_text(&mut dom, "Paused").await;
    // The form, typed into and left, is its own island too.
    begin(&mut dom).await;
    for text in ["R", "Re", "Rev"] {
        input_control(&mut dom, "Name", text);
    }
    key_on(&mut dom, FORM, Key::Escape);
    assert_eq!((props.probe.application.get(), props.probe.timeline.get(), props.probe.composer.get()), (application, timeline, composer), "task updates repaint the page alone");
    // Leaving the page ends the subscription: a change made now reaches nobody here.
    state.view().set(View::Chat);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    assert!(control(&dom, "Edit First").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_palette_opens_the_page() {
    let fixture = Fixture::start(false).await;
    let (mut dom, _props, state) = open(&fixture, &OPERATE).await;
    state.view().set(View::Chat);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    open_palette(&mut dom).await;
    input_control(&mut dom, PALETTE, "scheduled tasks");
    until(&mut dom, "the settings entry", |dom| control(dom, "Settings: Scheduled tasks").is_some()).await;
    click_control(&mut dom, "Settings: Scheduled tasks");
    until(&mut dom, "the page", |dom| rendered_text(dom).contains("Tasks of")).await;
    assert_eq!(state.peek().view, View::ScheduledTasks);
    assert!(control(&dom, PALETTE).is_none());
    fixture.finish(dom).await;
}
