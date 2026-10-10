//! Usage-limit recovery through the mounted UI and the real native server over the fake Codex:
//! the banner a limit puts over the composer, the choices it saves on the server, the server's own
//! "Continue where you left off." after the reset (the server's manual clock is moved by the test),
//! the manual "Resume thread", and the two settings that arm a recovery on their own.
//! Waits are on text or controls that only the awaited step produces.
use super::diff_tests::{Fixture, completed_runs, error, open_thread, rendered_text, seed, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, control, control_attribute, input_control};

/// The fake Codex's limit resets here, an hour after its turn: in the future for the client's real clock.
const RESET: &str = "2027-01-15T09:00:00.000Z";
const RESET_MS: i64 = 1_800_003_600_000;

fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}
fn shell(state: Store<UiModel>) -> t3_contracts::ThreadShell {
    state.peek().shell.snapshot.as_ref().unwrap().threads.iter().find(|thread| thread.id.as_str() == "thread-a").cloned().unwrap()
}
/// (auto-resume, snooze) as the thread's recovery has them; a thread with none has neither.
fn recovery(state: Store<UiModel>) -> (bool, bool) {
    shell(state).limit_recovery.flatten().map_or((false, false), |recovery| (recovery.auto_resume, recovery.snooze.flatten().unwrap_or(false)))
}
/// A limited thread, opened: `thread-a` after a turn the fake Codex stopped on a usage limit.
async fn limited(fixture: &Fixture) -> (VirtualDom, super::diff_tests::Harness, Store<UiModel>) {
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    input_control(&mut dom, "Message", "[usage-limit] go");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the banner for the limit", |dom| control(dom, "Resume at reset").is_some()).await;
    (dom, props, state)
}

#[tokio::test(flavor = "current_thread")]
async fn the_banner_saves_the_choices_on_the_server_and_the_reset_resumes_the_thread() {
    let fixture = Fixture::start_timed(json!({}), &[]).await;
    let (mut dom, props, state) = limited(&fixture).await;
    let text = rendered_text(&dom);
    // The banner is built from the shell: the reset the server reports, in the user's local time.
    assert!(text.contains("Usage limit reached") && text.contains("Resets 1/15/2027"), "{text}");
    assert_eq!(shell(state).usage_limit_reset_at.flatten().map(|at| at.to_string()).as_deref(), Some(RESET));
    assert_eq!(recovery(state), (false, false));
    // The sidebar says the thread is limited.
    assert_eq!(t3_client::sidebar::status(&shell(state)), t3_client::sidebar::Status::Limited);

    // A choice is sent once: disabled from the click until the server has answered, and then the scheduled state shows.
    click_control(&mut dom, "Resume at reset");
    assert!(disabled(&dom, "Resume at reset") && disabled(&dom, "Snooze until reset"), "both wait for the answer");
    until(&mut dom, "the scheduled state", |dom| control(dom, "Cancel auto-resume").is_some() && !disabled(dom, "Cancel auto-resume")).await;
    assert_eq!(recovery(state), (true, false));
    assert_eq!(error(state), None);

    // Cancelling clears it.
    click_control(&mut dom, "Cancel auto-resume");
    until(&mut dom, "the choice offered again", |dom| control(dom, "Resume at reset").is_some() && !disabled(dom, "Resume at reset")).await;
    assert_eq!(recovery(state), (false, false));

    // Snoozing until the reset puts the thread to sleep on the recovery's account; the snooze choice is spent.
    click_control(&mut dom, "Snooze until reset");
    until(&mut dom, "the snooze", |dom| control(dom, "Snooze until reset").is_none() && !disabled(dom, "Resume at reset")).await;
    assert_eq!(recovery(state), (false, true));
    assert_eq!(shell(state).snoozed_until.flatten().map(|at| at.0.timestamp_millis()), Some(RESET_MS));

    // Both, then the reset passes: the server itself sends the continuation.
    click_control(&mut dom, "Resume at reset");
    until(&mut dom, "the schedule", |dom| control(dom, "Cancel auto-resume").is_some() && !disabled(dom, "Cancel auto-resume")).await;
    assert_eq!(recovery(state), (true, true));
    fixture.clock.as_ref().unwrap().set(RESET_MS + 1_000);
    // The thread has a second run, started by the server with its own words. (The fake Codex then needs
    // a provider process the idle release let go of, which the clock jump makes slow, so this goes no further.)
    until(&mut dom, "the continuation to start", |_| state.peek().thread.projection.as_ref().is_some_and(|p| p["runs"].as_array().is_some_and(|runs| runs.len() == 2))).await;
    until(&mut dom, "the continuation in the conversation", |dom| rendered_text(dom).contains("Continue where you left off.")).await;
    let messages = state.peek().thread.projection.as_ref().unwrap()["messages"].clone();
    let sent = messages.as_array().unwrap().iter().find(|message| message["text"] == "Continue where you left off.").unwrap().clone();
    assert_eq!((sent["createdBy"].as_str(), sent["creationSource"].as_str()), (Some("user"), Some("server")));
    // It no longer stops on the limit, so the banner has gone with it.
    until(&mut dom, "the banner to go", |dom| control(dom, "Cancel auto-resume").is_none() && control(dom, "Resume at reset").is_none()).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_limited_thread_is_resumed_by_hand_with_the_run_it_continues() {
    let fixture = Fixture::start_timed(json!({}), &[]).await;
    let (mut dom, props, state) = limited(&fixture).await;
    // With nothing typed, the send button is "Resume thread".
    until(&mut dom, "the resume", |dom| control(dom, "Resume thread").is_some()).await;
    assert!(control(&dom, "Resume thread").is_some() && control(&dom, "Submit message").is_none());
    // Typing turns it back into a send; clearing the box offers the resume again.
    input_control(&mut dom, "Message", "something else");
    assert!(control(&dom, "Resume thread").is_none() && control(&dom, "Submit message").is_some());
    input_control(&mut dom, "Message", "");
    assert!(control(&dom, "Resume thread").is_some());
    click_control(&mut dom, "Resume thread");
    until(&mut dom, "the resumed turn", |_| completed_runs(state) == 1).await;
    let projection = state.peek().thread.projection.clone().unwrap();
    // The server accepts a continuation only from the thread's latest executed run, and the limited run is that.
    assert_eq!(projection["runs"].as_array().unwrap().iter().filter(|run| run["status"] == "failed").count(), 1);
    let messages = projection["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| message["text"] == "Continue where you left off." && message["createdBy"] == "user"));
    // (The fake Codex stamps its turns in 2027, ahead of this clock, so the limited run still reads as the latest
    // one here; on a real timeline the resumed run is newer and the banner and resume go.)
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];

fn setting(state: Store<UiModel>, key: &str) -> Value {
    state.peek().config["settings"][key].clone()
}

#[tokio::test(flavor = "current_thread")]
async fn the_general_page_sets_what_the_server_does_with_a_limited_thread() {
    let fixture = Fixture::start_timed(json!({}), &[]).await;
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some() && state.peek().config["settings"].is_object()).await;
    state.view().set(View::General);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the page, once the grant is known", |dom| control(dom, "Auto-resume limited threads").is_some() && !disabled(dom, "Auto-resume limited threads")).await;
    assert_eq!((setting(state, "autoResumeLimitedThreads"), setting(state, "snoozeLimitedThreads")), (json!(false), json!(false)));
    // One patch per change; the switch waits for the server and then shows what the server says.
    change_control(&mut dom, "Auto-resume limited threads", "true");
    assert!(disabled(&dom, "Auto-resume limited threads"), "disabled while the change is in flight");
    until(&mut dom, "auto-resume saved", |_| setting(state, "autoResumeLimitedThreads") == json!(true)).await;
    until(&mut dom, "the switch free", |dom| !disabled(dom, "Auto-resume limited threads")).await;
    change_control(&mut dom, "Snooze limited threads", "true");
    until(&mut dom, "snooze saved", |_| setting(state, "snoozeLimitedThreads") == json!(true)).await;
    until(&mut dom, "the switch free", |dom| !disabled(dom, "Snooze limited threads")).await;
    assert_eq!(error(state), None);

    // The settle settings: the server advertises auto-settlement, so the rows are there. Off writes null, on writes the default days.
    assert_eq!(setting(state, "sidebarAutoSettleAfterDays"), json!(3));
    assert!(control(&dom, "Days of inactivity before auto-settle").is_some());
    change_control(&mut dom, "Auto-settle inactive threads", "false");
    until(&mut dom, "inactive settling off", |_| setting(state, "sidebarAutoSettleAfterDays").is_null()).await;
    until(&mut dom, "the days field gone", |dom| control(dom, "Days of inactivity before auto-settle").is_none() && !disabled(dom, "Auto-settle inactive threads")).await;
    change_control(&mut dom, "Auto-settle inactive threads", "true");
    until(&mut dom, "inactive settling on", |_| setting(state, "sidebarAutoSettleAfterDays") == json!(3)).await;
    until(&mut dom, "the days field", |dom| control(dom, "Days of inactivity before auto-settle").is_some() && !disabled(dom, "Days of inactivity before auto-settle")).await;
    input_control(&mut dom, "Days of inactivity before auto-settle", "7");
    until(&mut dom, "seven days", |_| setting(state, "sidebarAutoSettleAfterDays") == json!(7)).await;
    until(&mut dom, "the field free", |dom| !disabled(dom, "Days of inactivity before auto-settle")).await;
    // Out of range or fractional counts are not sent.
    for bad in ["0", "91", "3.5"] {
        input_control(&mut dom, "Days of inactivity before auto-settle", bad);
        assert_eq!(setting(state, "sidebarAutoSettleAfterDays"), json!(7), "{bad} is refused");
    }
    change_control(&mut dom, "Auto-settle merged threads", "false");
    until(&mut dom, "merge settling off", |_| setting(state, "sidebarAutoSettleOnMerge") == json!(false)).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_the_settings_grant_the_general_page_changes_nothing() {
    let fixture = Fixture::start_timed(json!({}), &[]).await;
    let (mut dom, props) = fixture.client(Gate::default(), &[AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate]);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["settings"].is_object()).await;
    state.view().set(View::General);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    super::diff_tests::wait_for_rendered_text(&mut dom, "This connection can view these settings but cannot change them.").await;
    assert!(disabled(&dom, "Auto-resume limited threads") && disabled(&dom, "Snooze limited threads") && disabled(&dom, "Auto-settle merged threads"));
    assert!(rendered_text(&dom).contains("This connection can view these settings but cannot change them."));
    fixture.finish(dom).await;
}
