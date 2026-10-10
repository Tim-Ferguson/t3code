//! Forking, merging back and the lineage panel through the mounted UI and the real native server
//! with the fake Codex, and what a fork's long inherited history costs to open. Waits are on
//! text or controls that only the awaited step produces.
use super::diff_tests::{Fixture, completed_runs, open_thread, rendered_text, seed, turn, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::key_on;
use crate::runtime::transport_tests::{all_controls, click_control, click_nth_control, control, control_attribute};

fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}
fn projection(state: Store<UiModel>) -> Value {
    state.peek().thread.projection.clone().unwrap()
}
fn forks_of(state: Store<UiModel>, title: &str) -> Vec<String> {
    state.peek().shell.snapshot.as_ref().unwrap().threads.iter().filter(|thread| thread.title == title).map(|thread| thread.id.to_string()).collect()
}
fn transfers(state: Store<UiModel>, kind: &str) -> Vec<Value> {
    projection(state)["contextTransfers"].as_array().unwrap().iter().filter(|transfer| transfer["type"] == kind).cloned().collect()
}

#[tokio::test(flavor = "current_thread")]
async fn a_response_is_forked_once_the_fork_opens_and_merges_back_into_its_parent() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    turn(&mut dom, &props, state, 2).await;
    until(&mut dom, "a fork button on each answer", |dom| all_controls(dom, "Fork from this response").len() == 2).await;
    assert!(!rendered_text(&dom).contains("Thread lineage"), "a thread with no relatives has no lineage");
    assert!(control(&dom, "Thread lineage").is_none());

    // The first answer is forked; a second click while the write is in flight sends nothing.
    click_nth_control(&mut dom, "Fork from this response", 0);
    click_nth_control(&mut dom, "Fork from this response", 0);
    until(&mut dom, "the fork opened", |_| {
        let model = state.peek();
        model.active_thread.as_deref().is_some_and(|id| id != "thread-a") && model.thread.synchronized && model.thread.projection.as_ref().is_some_and(|p| p["thread"]["lineage"]["relationshipToParent"] == "fork")
    })
    .await;
    assert_eq!(forks_of(state, "thread-a fork").len(), 1, "one click, one fork: {:?}", state.peek().shell.snapshot.as_ref().unwrap().threads.iter().map(|t| (t.id.to_string(), t.title.clone())).collect::<Vec<_>>());
    let fork_id = state.peek().active_thread.clone().unwrap();
    until(&mut dom, "the fork's marker", |dom| control(dom, "Open source conversation").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Forked from conversation") && text.contains("Do the work 1") && !text.contains("Do the work 2"), "the fork shows the history up to its point: {text}");
    let rows = projection(state)["visibleTurnItems"].as_array().unwrap().clone();
    assert!(rows.iter().any(|row| row["visibility"] == "inherited") && rows.iter().any(|row| row["visibility"] == "synthetic" && row["item"]["type"] == "fork"));
    // Nothing has run in the fork, so there is nothing to merge back yet.
    until(&mut dom, "the lineage button", |dom| control(dom, "Thread lineage").is_some()).await;
    click_control(&mut dom, "Thread lineage");
    assert!(rendered_text(&dom).contains("Do the work 1"), "{}", rendered_text(&dom));
    assert!(disabled(&dom, "Merge back to thread-a"), "no finished run in the fork yet");
    key_on(&mut dom, "Thread lineage", Key::Escape);
    assert!(control(&dom, "Merge back to thread-a").is_none(), "Escape closes the panel");

    // The fork goes on on its own, then merges back; the parent is opened.
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "Fork work");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the fork's turn", |_| completed_runs(state) == 1).await;
    // A message to an existing thread seeds with its own title: the fork keeps its name, not the message's.
    let titles: Vec<String> = state.peek().shell.snapshot.as_ref().unwrap().threads.iter().map(|thread| thread.title.clone()).collect();
    assert!(titles.iter().any(|title| title == "thread-a fork") && !titles.iter().any(|title| title == "Fork work"), "the fork keeps its title after its first message: {titles:?}");
    click_control(&mut dom, "Thread lineage");
    until(&mut dom, "merge enabled", |dom| control(dom, "Merge back to thread-a").is_some() && !disabled(dom, "Merge back to thread-a")).await;
    click_control(&mut dom, "Merge back to thread-a");
    click_control(&mut dom, "Merge back to thread-a");
    until(&mut dom, "the parent opened", |_| {
        let model = state.peek();
        model.active_thread.as_deref() == Some("thread-a") && model.thread.synchronized && model.thread.projection.as_ref().is_some_and(|p| p["thread"]["id"] == "thread-a")
    })
    .await;
    let merges = transfers(state, "merge_back");
    assert_eq!(merges.len(), 1, "one click, one merge");
    assert_eq!((merges[0]["status"].as_str(), merges[0]["sourceThreadId"].as_str()), (Some("pending"), Some(fork_id.as_str())));
    // The parent lists its fork, found from the parent as the parent is from the fork.
    // Text only the parent has: its second answer, which the fork never inherited.
    until(&mut dom, "the parent's conversation", |dom| rendered_text(dom).contains("Fixture response: Do the work 2") && control(dom, "Thread lineage").is_some()).await;
    click_control(&mut dom, "Thread lineage");
    until(&mut dom, "the fork in the parent's lineage", |dom| control(dom, "thread-a fork*").is_some()).await;
    key_on(&mut dom, "Thread lineage", Key::Escape);

    // The next message to the parent carries the fork's work, and the timeline says so.
    turn(&mut dom, &props, state, 3).await;
    until(&mut dom, "the handoff", |dom| rendered_text(dom).contains("Context handoff") && rendered_text(dom).contains("Merge-back context")).await;
    assert_eq!(transfers(state, "merge_back")[0]["status"], "consumed");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_fork_says_why_and_can_be_tried_again() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    until(&mut dom, "the fork button", |dom| control(dom, "Fork from this response").is_some()).await;
    // A run the thread never had: the server refuses in the original's words.
    dom.in_scope(ScopeId::APP, || crate::thread_relations::fork(&props.transport, state, "thread-a".into(), "run:nope".into()));
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("No stable source run was found for fork source run.")).await;
    assert!(state.peek().relations.fork.is_none(), "the button is free again");
    assert!(!disabled(&dom, "Fork from this response"));
    assert_eq!(state.peek().active_thread.as_deref(), Some("thread-a"), "a refused fork opens nothing");
    assert!(forks_of(state, "thread-a fork").is_empty());
    click_control(&mut dom, "Fork from this response");
    until(&mut dom, "the fork", |_| state.peek().active_thread.as_deref().is_some_and(|id| id != "thread-a")).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn without_the_operate_grant_forking_and_merging_are_off() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    drop(dom);
    let (mut reader, props) = fixture.client(Gate::default(), &[t3_contracts::AuthEnvironmentScope::OrchestrationRead]);
    let read_state = props.state.borrow().unwrap();
    until(&mut reader, "shell", |_| read_state.peek().shell.synchronized && read_state.peek().shell.snapshot.as_ref().is_some_and(|s| s.threads.len() == 2)).await;
    open_thread(&mut reader, &props, read_state, "thread-a").await;
    until(&mut reader, "the fork button", |dom| control(dom, "Fork from this response").is_some()).await;
    assert!(disabled(&reader, "Fork from this response"));
    drop(reader);
    fixture.finish(VirtualDom::new(|| rsx! {})).await;
}


fn shell_thread(state: Store<UiModel>, id: &str) -> Value {
    serde_json::to_value(state.peek().shell.snapshot.as_ref().unwrap().threads.iter().find(|thread| thread.id.as_str() == id).unwrap()).unwrap()
}
fn watched(state: Store<UiModel>, id: &str) -> bool {
    shell_thread(state, id)["pullRequests"].as_array().is_some_and(|links| links.iter().any(|link| link["watch"].is_object()))
}

#[tokio::test(flavor = "current_thread")]
async fn a_queued_message_is_steered_into_the_running_turn_once_and_not_without_one() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "work [hold]");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "a turn the provider is running", |_| {
        let projection = state.peek().thread.projection.clone();
        projection.is_some_and(|p| p["providerTurns"].as_array().is_some_and(|turns| turns.iter().any(|turn| turn["status"] == "running")) && t3_client::queue_workflow::can_promote_to_steer(&p))
    })
    .await;
    dom.in_scope(ScopeId::APP, || send_with(&props.transport, state, SendOptions { mode: Some("queue"), text: Some("queued two".into()), ..Default::default() }));
    until(&mut dom, "the queued message", |dom| control(dom, "Steer queued message").is_some()).await;
    assert!(!disabled(&dom, "Steer queued message"), "a running turn can be steered");
    // Two clicks, one steer.
    click_control(&mut dom, "Steer queued message");
    click_control(&mut dom, "Steer queued message");
    until(&mut dom, "the steer", |_| projection_items(state, "queued two").iter().any(|item| item["inputIntent"] == "promoted_queued_to_steer")).await;
    assert_eq!(projection_items(state, "queued two").len(), 1);
    until(&mut dom, "the queue empty", |dom| control(dom, "Steer queued message").is_none()).await;
    until(&mut dom, "the turn finished", |_| state.peek().relations.steering.is_none() && completed_runs(state) >= 1).await;

    // With nothing running there is nothing to steer into: the queue holds, and the button says why.
    dom.in_scope(ScopeId::APP, || send_with(&props.transport, state, SendOptions { mode: Some("queue"), text: Some("queued three".into()), ..Default::default() }));
    until(&mut dom, "the turn", |_| completed_runs(state) >= 2).await;
    fixture.finish(dom).await;
}
/// The keyboard path of the same steer: the shortcut sends the first queued message into the running
/// turn, once, claimed only while a queued message and a steerable turn exist.
#[tokio::test(flavor = "current_thread")]
async fn the_steer_shortcut_sends_the_first_queued_message_into_the_running_turn() {
    use crate::runtime::keys_tests::{chord, press};
    let none = t3_client::keybindings::Dom::default;
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    // Nothing queued: the chord is not claimed, so it keeps its meaning (a newline in the box).
    assert_eq!(press(&mut dom, chord("Enter", true, false), none()).await, None);
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "work [hold]");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "a turn the provider is running", |_| {
        let projection = state.peek().thread.projection.clone();
        projection.is_some_and(|p| p["providerTurns"].as_array().is_some_and(|turns| turns.iter().any(|turn| turn["status"] == "running")) && t3_client::queue_workflow::can_promote_to_steer(&p))
    })
    .await;
    dom.in_scope(ScopeId::APP, || send_with(&props.transport, state, SendOptions { mode: Some("queue"), text: Some("queued by key".into()), ..Default::default() }));
    until(&mut dom, "the queued message", |dom| control(dom, "Steer queued message").is_some()).await;
    assert_eq!(press(&mut dom, chord("Enter", true, false), none()).await.as_deref(), Some("thread.steerQueuedMessage"));
    until(&mut dom, "the steer", |_| projection_items(state, "queued by key").iter().any(|item| item["inputIntent"] == "promoted_queued_to_steer")).await;
    assert_eq!(projection_items(state, "queued by key").len(), 1);
    until(&mut dom, "the queue empty", |dom| control(dom, "Steer queued message").is_none()).await;
    fixture.finish(dom).await;
}
fn projection_items(state: Store<UiModel>, text: &str) -> Vec<Value> {
    projection(state)["turnItems"].as_array().unwrap().iter().filter(|item| item["type"] == "user_message" && item["text"] == text).cloned().collect()
}

#[tokio::test(flavor = "current_thread")]
async fn a_watched_pull_request_is_background_work_with_a_stop_that_lets_it_go() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let link = json!({"threadId":"thread-a","host":"github.com","repository":"acme/web","number":7,"url":"https://github.com/acme/web/pull/7","source":"manual"});
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.pull-request.link", link.clone()));
    until(&mut dom, "the link", |dom| control(dom, "Linked pull request #7").is_some() || rendered_text(dom).contains("#7")).await;
    let watch = |watching: bool| json!({"threadId":"thread-a","host":"github.com","repository":"acme/web","number":7,"watching":watching});
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.pull-request.watch", watch(true)));
    until(&mut dom, "the watch", |dom| watched(state, "thread-a") && control(dom, "Stop watching #7").is_some()).await;
    // It is work the agent is waiting on, in the strip every other background task uses.
    until(&mut dom, "the strip", |dom| rendered_text(dom).contains("Watching pull request #7")).await;

    // The eye lets it go, once however often it is clicked.
    click_control(&mut dom, "Stop watching #7");
    click_control(&mut dom, "Stop watching #7");
    until(&mut dom, "the watch ended", |dom| !watched(state, "thread-a") && control(dom, "Stop watching #7").is_none() && !rendered_text(dom).contains("Watching pull request #7")).await;

    // The thread's Stop does the same when no run is left to interrupt.
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.pull-request.watch", watch(true)));
    until(&mut dom, "the watch again", |_| watched(state, "thread-a")).await;
    until(&mut dom, "the strip again", |dom| control(dom, "Stop background work").is_some()).await;
    dom.in_scope(ScopeId::APP, || stop_thread(&props.transport, state, "thread-a"));
    until(&mut dom, "let go by Stop", |_| !watched(state, "thread-a")).await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn choosing_a_model_on_another_provider_hands_the_conversation_over_and_the_timeline_says_so() {
    use crate::runtime::keys_tests::{chord, press};
    let fixture = Fixture::start_claude(&[("CLAUDE_FIXTURE_SCENARIO", "turn")], &[]).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    assert_eq!(press(&mut dom, chord("m", true, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.toggle"));
    until(&mut dom, "the picker", |dom| control(dom, "Search models").is_some()).await;
    crate::runtime::transport_tests::input_control(&mut dom, "Search models", "claude");
    // The first Claude row is picked by its number. Nothing is written: the choice rides on the next message.
    assert_eq!(press(&mut dom, chord("1", false, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.jump.1"));
    until(&mut dom, "the chosen model", |dom| control(dom, "Model").and_then(|(_, value)| value).is_some_and(|value| value.contains("claude"))).await;
    assert!(state.peek().error.is_none(), "a started thread may change provider when its session can hand over: {:?}", state.peek().error);
    assert_eq!(shell_thread(state, "thread-a")["providerInstanceId"], "codex");
    turn(&mut dom, &props, state, 2).await;
    until(&mut dom, "the provider switched", |_| shell_thread(state, "thread-a")["providerInstanceId"] == "claudeAgent").await;
    let projection = projection(state);
    assert_eq!(projection["contextTransfers"].as_array().unwrap().iter().filter(|transfer| transfer["type"] == "provider_handoff").count(), 1);
    // The timeline says what moved where: the row is the original's "Context handoff" between the two providers.
    until(&mut dom, "the handoff row", |dom| control(dom, "Context handoff").is_some()).await;
    let text = rendered_text(&dom);
    assert!(text.contains("Context handoff") && text.contains("Fixture Model") && text.contains("Claude Opus 4.7") && text.contains("Provider handoff"), "{text}");
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_message_to_another_provider_is_refused_while_a_run_is_active_in_the_servers_words() {
    use crate::runtime::keys_tests::{chord, press};
    let fixture = Fixture::start_claude(&[("CLAUDE_FIXTURE_SCENARIO", "turn")], &[]).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "work [hold]");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "a turn the provider is running", |_| projection(state)["providerTurns"].as_array().is_some_and(|turns| turns.iter().any(|turn| turn["status"] == "running"))).await;
    assert_eq!(press(&mut dom, chord("m", true, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.toggle"));
    until(&mut dom, "the picker", |dom| control(dom, "Search models").is_some()).await;
    crate::runtime::transport_tests::input_control(&mut dom, "Search models", "claude");
    assert_eq!(press(&mut dom, chord("1", false, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.jump.1"));
    until(&mut dom, "the chosen model", |dom| control(dom, "Model").and_then(|(_, value)| value).is_some_and(|value| value.contains("claude"))).await;
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "and now?");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("A message to another provider cannot steer or restart a run that is still active.")).await;
    assert_eq!(shell_thread(state, "thread-a")["providerInstanceId"], "codex", "nothing switched");
    dom.in_scope(ScopeId::APP, || stop_thread(&props.transport, state, "thread-a"));
    fixture.finish(dom).await;
}

/// A message to another provider while a run is active is queued on that provider (the original sends the
/// model with the message and the server decides): the queue shows it, and once the active turn is stopped and the queue resumed it runs on the other
/// provider, which the timeline shows as a handoff.
#[tokio::test(flavor = "current_thread")]
async fn a_message_to_another_provider_queues_on_it_while_a_run_is_active_and_runs_there_afterwards() {
    use crate::runtime::keys_tests::{chord, press};
    let fixture = Fixture::start_claude(&[("CLAUDE_FIXTURE_SCENARIO", "turn")], &[]).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "work [hold]");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "a turn the provider is running", |_| {
        let projection = state.peek().thread.projection.clone();
        projection.is_some_and(|p| p["providerTurns"].as_array().is_some_and(|turns| turns.iter().any(|turn| turn["status"] == "running")) && t3_client::queue_workflow::can_promote_to_steer(&p))
    })
    .await;
    // The picker offers the other provider while the turn runs, because the session can hand over; nothing is sent yet.
    assert_eq!(press(&mut dom, chord("m", true, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.toggle"));
    until(&mut dom, "the picker", |dom| control(dom, "Search models").is_some()).await;
    crate::runtime::transport_tests::input_control(&mut dom, "Search models", "claude");
    assert_eq!(press(&mut dom, chord("1", false, false), t3_client::keybindings::Dom::default()).await.as_deref(), Some("modelPicker.jump.1"));
    until(&mut dom, "the chosen model", |dom| control(dom, "Model").and_then(|(_, value)| value).is_some_and(|value| value.contains("claude"))).await;
    assert!(state.peek().error.is_none(), "{:?}", state.peek().error);
    assert_eq!(shell_thread(state, "thread-a")["providerInstanceId"], "codex", "choosing writes nothing");
    // Queued, it is accepted and waits on the other provider.
    dom.in_scope(ScopeId::APP, || send_with(&props.transport, state, SendOptions { mode: Some("queue"), text: Some("and now?".into()), ..Default::default() }));
    until(&mut dom, "the queued message", |dom| control(dom, "Steer queued message").is_some()).await;
    assert!(state.peek().error.is_none(), "{:?}", state.peek().error);
    let queued = projection(state)["runs"].as_array().unwrap().iter().filter(|run| run["status"] == "queued").cloned().collect::<Vec<_>>();
    assert_eq!((queued.len(), queued[0]["providerInstanceId"].as_str()), (1, Some("claudeAgent")), "{queued:?}");
    assert_eq!(shell_thread(state, "thread-a")["providerInstanceId"], "codex", "the thread stays on the active provider until the queued run starts");
    // Steering it into the run that is still active is refused by the server, in its words.
    // Stopping the active turn holds the queue; resuming delivers it on the other provider.
    dom.in_scope(ScopeId::APP, || stop_thread(&props.transport, state, "thread-a"));
    until(&mut dom, "the queue held", |dom| control(dom, "Resume thread").is_some()).await;
    click_control(&mut dom, "Resume thread");
    until(&mut dom, "the run on the other provider", |_| shell_thread(state, "thread-a")["providerInstanceId"] == "claudeAgent" && projection(state)["runs"].as_array().unwrap().iter().any(|run| run["providerInstanceId"] == "claudeAgent" && run["status"] == "completed")).await;
    until(&mut dom, "the handoff row", |dom| control(dom, "Context handoff").is_some()).await;
    fixture.finish(dom).await;
}

/// A message to a provider the environment has turned off is answered in the server's words and shown, and the
/// send is over rather than a spinner. (The "target provider instance is unavailable" refusal belongs to
/// `thread.model-selection.set` and `provider.switch`, which this composer, like the original's, never sends: the
/// model rides on the message.)
#[tokio::test(flavor = "current_thread")]
async fn a_message_to_a_disabled_provider_is_answered_in_the_servers_words() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    // The picker does not offer a disabled instance; a choice made before it was turned off still rides on the message.
    let key = (state.peek().destination.clone().unwrap(), "thread-a".to_owned());
    let mut choices = state.peek().thread_choices.get(&key).cloned().unwrap_or_default();
    choices.model_selection = Some(serde_json::from_value(json!({"instanceId":"claudeAgent","model":"claude-opus-4-7"})).unwrap());
    state.thread_choices().write().insert(key, choices);
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "and now?");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the refusal", |dom| rendered_text(dom).contains("Claude is disabled in T3 Code settings.")).await;
    assert!(state.peek().pending_messages.is_empty(), "the send is over");
    fixture.finish(dom).await;
}

#[derive(Clone)]
pub(super) struct Probed {
    pub(super) inner: super::diff_tests::Harness,
    pub(super) probe: crate::RenderProbe,
}
pub(super) fn probed_app(props: Probed) -> Element {
    use_context_provider(|| props.probe.clone());
    super::diff_tests::harness(props.inner.clone())
}

/// The header's lineage button and a response's fork button repaint on what concerns them: a fork in
/// flight is the fork buttons' alone, opening the panel is the lineage button's alone, and a turn
/// streaming into the conversation builds neither again; the application, the conversation and the
/// composer are not built again by the first two.
#[tokio::test(flavor = "current_thread")]
async fn the_lineage_button_and_the_fork_button_repaint_alone() {
    let fixture = Fixture::start(false).await;
    let (_, props) = fixture.operator(Gate::default());
    let probe = crate::RenderProbe::default();
    let mut dom = VirtualDom::new_with_props(probed_app, Probed { inner: props.clone(), probe: probe.clone() });
    dom.rebuild_in_place();
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    until(&mut dom, "a fork button", |dom| all_controls(dom, "Fork from this response").len() == 1).await;
    click_control(&mut dom, "Fork from this response");
    until(&mut dom, "the fork opened", |dom| state.peek().active_thread.as_deref().is_some_and(|id| id != "thread-a") && control(dom, "Open source conversation").is_some() && control(dom, "Thread lineage").is_some()).await;
    // The fork's own opening has been delivered; what follows is measured.
    let counts = || ((probe.application.get(), probe.timeline.get(), probe.composer.get()), probe.lineage_button.get(), probe.fork_button.get());
    let (shell, lineage, fork) = counts();

    // A fork in flight is the fork buttons' alone.
    state.relations().write().fork = Some(crate::thread_relations::Busy { destination: "diff-ui".into(), thread: "x".into() });
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let (shell_after, lineage_after, fork_after) = counts();
    assert_eq!((shell_after, lineage_after), (shell, lineage), "the application, the conversation, the composer and the lineage button are not built again");
    let waiting = all_controls(&dom, "Fork from this response").len();
    assert_eq!(fork_after, fork + waiting, "each fork button waits, once");
    if waiting > 0 {
        assert_eq!(control_attribute(&dom, "Fork from this response", "aria-busy").and_then(|(_, value)| value).as_deref(), Some("true"));
    }
    state.relations().write().fork = None;
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    let (shell, lineage, fork) = counts();

    // The panel opens and closes in the lineage button alone.
    click_control(&mut dom, "Thread lineage");
    let (shell_after, lineage_after, fork_after) = counts();
    assert!(rendered_text(&dom).contains("Do the work 1"));
    assert_eq!((shell_after, fork_after), (shell, fork), "opening the panel");
    assert!(lineage_after > lineage);
    click_control(&mut dom, "Thread lineage");
    let (_, lineage, fork) = counts();

    // A turn streaming into the fork builds its rows and its new answer's footer, not the lineage button
    // nor the fork buttons that are already there.
    crate::runtime::transport_tests::input_control(&mut dom, "Message", "Fork work");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the new answer's fork button", |dom| rendered_text(dom).contains("Fixture response: Fork work") && !all_controls(dom, "Fork from this response").is_empty() && state.peek().thread.projection.as_ref().is_some_and(|p| p["runs"].as_array().is_some_and(|runs| runs.iter().all(|run| run["status"] == "completed")))).await;
    let (_, lineage_after, fork_after) = counts();
    assert_eq!(lineage_after, lineage, "the lineage button was not built again by the turn");
    assert!(fork_after <= fork + 2, "the answers' buttons were built once each, not over and over: {fork} then {fork_after}");
    fixture.finish(dom).await;
}
