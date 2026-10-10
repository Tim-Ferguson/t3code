//! Settling, snoozing and their quiet line through the mounted UI and the real native server over the
//! fake Codex: a settle works on a thread whose provider session is live, writing to a settled or
//! snoozed thread brings it back, the opt-out from automatic settling, and the server's own sweep
//! (the server's manual clock is moved by the test). Waits are on state or text that only the
//! awaited step produces.
use super::diff_tests::{Fixture, completed_runs, error, open_thread, rendered_text, seed, turn, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::key_on;
use crate::runtime::transport_tests::{all_controls, click_control, control, control_attribute};

fn thread(state: Store<UiModel>) -> t3_contracts::ThreadShell {
    state.peek().shell.snapshot.as_ref().unwrap().threads.iter().find(|thread| thread.id.as_str() == "thread-a").cloned().unwrap()
}
fn settled(state: Store<UiModel>) -> bool {
    thread(state).settled_override == Some(t3_contracts::SettledOverride::Settled)
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}
/// A message to thread-a whose words are its title from then on ("Quiet work"), because a thread's first
/// message titles it: the menu is found by that title, and no other thread shares it.
async fn say(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>, number: usize) {
    crate::runtime::transport_tests::input_control(dom, "Message", "Quiet work");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(dom, "the turn", |_| completed_runs(state) == number).await;
}
fn menu(dom: &mut VirtualDom) {
    key_on(dom, "Thread Quiet work", Key::ContextMenu);
}
/// Picks a choice of the open menu once its row has the server's answer to the last write: the event that
/// shows a change can reach the page before the answer does, and until then the row keeps its choices off,
/// where a click from a person does nothing.
async fn choose(dom: &mut VirtualDom, label: &str) {
    until(dom, label, |dom| control(dom, label).is_some() && !disabled(dom, label)).await;
    click_control(dom, label);
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_with_a_live_session_settles_and_its_line_and_a_message_bring_it_back() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    say(&mut dom, &props, state, 1).await;
    assert!(control(&dom, "Un-settle").is_none(), "no line while the thread is active");
    // The menu offers the settle with the session still live; the server releases the session itself.
    menu(&mut dom);
    assert!(control(&dom, "Settle thread").is_some() && !disabled(&dom, "Settle thread"));
    click_control(&mut dom, "Settle thread");
    until(&mut dom, "the settled line", |dom| control(dom, "Un-settle").is_some()).await;
    assert!(settled(state) && rendered_text(&dom).contains("Settled just now"), "{}", rendered_text(&dom));
    assert_eq!(error(state), None);
    // The way out is on the line; it is sent once and says what it is doing.
    click_control(&mut dom, "Un-settle");
    assert!(disabled(&dom, "Un-settle") && rendered_text(&dom).contains("Un-settling..."));
    until(&mut dom, "the thread back", |dom| !settled(state) && control(dom, "Un-settle").is_none()).await;
    // The menu says the same: Un-settle only for a settled one.
    menu(&mut dom);
    assert!(control(&dom, "Un-settle thread").is_none() && control(&dom, "Settle thread").is_some());
    click_control(&mut dom, "Settle thread");
    until(&mut dom, "settled again", |_| settled(state)).await;
    // Writing to a settled thread un-settles it and runs.
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "Do the work 2");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the second turn", |_| completed_runs(state) == 2).await;
    assert!(!settled(state));
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_snoozed_thread_says_so_wakes_on_request_and_is_woken_by_a_message() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let wake = (chrono::Utc::now() + chrono::Duration::hours(3) + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.snooze", json!({"threadId":"thread-a","snoozedUntil":wake}))).unwrap();
    until(&mut dom, "the snoozed line", |dom| control(dom, "Wake now").is_some()).await;
    assert!(rendered_text(&dom).contains("Snoozed, 3h left"), "{}", rendered_text(&dom));
    click_control(&mut dom, "Wake now");
    assert!(disabled(&dom, "Wake now"));
    until(&mut dom, "awake", |dom| control(dom, "Wake now").is_none() && thread(state).snoozed_until.flatten().is_none()).await;
    // A message to a snoozed thread wakes it as well.
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.snooze", json!({"threadId":"thread-a","snoozedUntil":wake}))).unwrap();
    until(&mut dom, "snoozed again", |dom| control(dom, "Wake now").is_some()).await;
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "Do the work 1");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the turn", |dom| completed_runs(state) == 1 && control(dom, "Wake now").is_none()).await;
    assert!(thread(state).snoozed_until.flatten().is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_can_opt_out_of_automatic_settling_and_the_sweep_settles_the_others() {
    // Two days of quiet settle a thread; the manual clock is moved past them.
    let fixture = Fixture::start_timed(json!({"sidebarAutoSettleAfterDays":2}), &[]).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    // thread-b has run once; thread-a opts out and has run once.
    open_thread(&mut dom, &props, state, "thread-b").await;
    turn(&mut dom, &props, state, 1).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    say(&mut dom, &props, state, 1).await;
    menu(&mut dom);
    // The choice is a setting with the current one marked; it is on by default.
    assert_eq!(control_attribute(&dom, "Auto-settle behavior: Enabled", "aria-checked").and_then(|(_, value)| value).as_deref(), Some("true"));
    assert_eq!(control_attribute(&dom, "Auto-settle behavior: Disabled", "aria-checked").and_then(|(_, value)| value).as_deref(), Some("false"));
    choose(&mut dom, "Auto-settle behavior: Disabled").await;
    until(&mut dom, "the opt-out", |_| thread(state).auto_settle_disabled_at.flatten().is_some()).await;
    menu(&mut dom);
    assert_eq!(control_attribute(&dom, "Auto-settle behavior: Disabled", "aria-checked").and_then(|(_, value)| value).as_deref(), Some("true"));
    choose(&mut dom, "Auto-settle behavior: Enabled").await;
    until(&mut dom, "enabled again", |_| thread(state).auto_settle_disabled_at.flatten().is_none()).await;
    menu(&mut dom);
    choose(&mut dom, "Auto-settle behavior: Disabled").await;
    until(&mut dom, "the opt-out again", |_| thread(state).auto_settle_disabled_at.flatten().is_some()).await;

    // Past the quiet days, the sweep settles the thread that did not opt out, and only it.
    fixture.clock.as_ref().unwrap().set(chrono::Utc::now().timestamp_millis() + 3 * 86_400_000);
    until(&mut dom, "the sweep", |_| state.peek().shell.snapshot.as_ref().unwrap().threads.iter().any(|t| t.id.as_str() == "thread-b" && t.settled_override == Some(t3_contracts::SettledOverride::Settled))).await;
    assert!(!settled(state), "thread-a opted out");
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

/// The status line repaints on what it shows alone: a turn streaming into the conversation builds it no more.
#[tokio::test(flavor = "current_thread")]
async fn the_status_line_is_not_built_again_by_a_turn() {
    let fixture = Fixture::start(false).await;
    let (_, props) = fixture.operator(Gate::default());
    let probe = crate::RenderProbe::default();
    let mut dom = VirtualDom::new_with_props(super::thread_relations_tests::probed_app, super::thread_relations_tests::Probed { inner: props.clone(), probe: probe.clone() });
    dom.rebuild_in_place();
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let built = probe.status_line.get();
    assert!(built >= 1);
    turn(&mut dom, &props, state, 1).await;
    assert_eq!(probe.status_line.get(), built, "{} shell events and a whole turn built the line no more", state.peek().shell.snapshot.as_ref().unwrap().snapshot_sequence);
    assert!(all_controls(&dom, "Un-settle").is_empty());
    fixture.finish(dom).await;
}

/// The `thread.settle` shortcut and the palette entry settle the open thread and bring it back,
/// where the server can settle and the connection may operate.
#[tokio::test(flavor = "current_thread")]
async fn the_settle_shortcut_and_the_palette_entry_toggle_the_open_thread() {
    use crate::runtime::keys_tests::{chord, open_palette, press};
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    assert_eq!(press(&mut dom, chord("s", true, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("thread.settle"));
    until(&mut dom, "settled by the shortcut", |_| settled(state)).await;
    open_palette(&mut dom).await;
    crate::runtime::transport_tests::input_control(&mut dom, crate::runtime::keys_tests::PALETTE, "settle");
    until(&mut dom, "the palette's entry", |dom| control(dom, "Un-settle thread").is_some()).await;
    click_control(&mut dom, "Un-settle thread");
    until(&mut dom, "brought back by the palette", |_| !settled(state)).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

/// The server's lifecycle stream on connect: threads being restored from the previous version are one notice for
/// as long as they are, and a first run's welcome opens the thread it bootstraps, once, from the start screen.
#[tokio::test(flavor = "current_thread")]
async fn the_lifecycle_stream_announces_a_restoration_and_opens_a_first_runs_thread() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    let publish = |kind: &str, payload: Value| t3_server::server_lifecycle::publish(&fixture.store, kind, payload);
    publish("legacyThreadMigration", json!({"status":"running","totalThreadCount":3}));
    until(&mut dom, "the restoration notice", |dom| rendered_text(dom).contains("Restoring your threads\u{2026}") && rendered_text(dom).contains("Migrating 3 threads from the previous version. You can keep working while this finishes.")).await;
    // A newer count replaces the words, not the notice.
    publish("legacyThreadMigration", json!({"status":"running","totalThreadCount":1200}));
    until(&mut dom, "the new count", |dom| rendered_text(dom).contains("Migrating 1,200 threads")).await;
    assert_eq!(state.peek().notices.iter().filter(|notice| notice.title.starts_with("Restoring")).count(), 1);
    publish("legacyThreadMigration", json!({"status":"complete","totalThreadCount":1200}));
    until(&mut dom, "the notice gone", |dom| !rendered_text(dom).contains("Restoring your threads")).await;

    // A welcome that names a thread opens it from the start screen, once.
    assert!(state.peek().active_thread.is_none());
    publish("welcome", json!({"environment":{"environmentId":"x","label":"x","platform":{"os":"linux","arch":"x64"},"serverVersion":"t","orchestrationProtocolVersion":2,"capabilities":{}},"cwd":"/w","projectName":"Project","bootstrapProjectId":"project","bootstrapThreadId":"thread-b"}));
    until(&mut dom, "the bootstrap thread", |_| state.peek().active_thread.as_deref() == Some("thread-b")).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    publish("welcome", json!({"environment":{"environmentId":"x","label":"x","platform":{"os":"linux","arch":"x64"},"serverVersion":"t","orchestrationProtocolVersion":2,"capabilities":{}},"cwd":"/w","projectName":"Project","bootstrapProjectId":"project","bootstrapThreadId":"thread-b"}));
    // Over a thread the user has open, it leaves the screen alone.
    for _ in 0..20 { let _ = tokio::time::timeout(std::time::Duration::from_millis(25), dom.wait_for_work()).await; dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations); }
    assert_eq!(state.peek().active_thread.as_deref(), Some("thread-a"));
    fixture.finish(dom).await;
}
