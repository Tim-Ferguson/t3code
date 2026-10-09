//! Composer context chips through the mounted UI and the real native server: a thread attached
//! from the sidebar, a comment written on a line of the diff, and chips saved by the original app;
//! each is shown, removed, saved with the draft, and sent as `context` on the message that
//! claims it. Waits are on text or controls that only the awaited step produces.
use super::diff_tests::{Fixture, completed_runs, open_thread, rendered_text, seed, turn, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::key_on;
use crate::runtime::transport_tests::{click_control, control, input_control};
use t3_client::draft_storage::DraftTarget;

fn target(thread: &str) -> DraftTarget {
    DraftTarget::thread("diff-ui", thread)
}
fn saved(state: Store<UiModel>, thread: &str) -> Vec<Value> {
    state.peek().draft_storage.document.borrow().contexts(&target(thread))
}
fn message_context(state: Store<UiModel>) -> Vec<Value> {
    let projection = state.peek().thread.projection.clone().unwrap();
    projection["messages"].as_array().unwrap().iter().filter(|message| message["role"] == "user" && message["context"].is_object()).flat_map(|message| message["context"]["records"].as_array().cloned().unwrap_or_default()).collect()
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_attached_from_the_sidebar_is_a_chip_that_is_saved_removed_and_sent_with_the_message() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;

    // The sidebar's menu on another thread attaches it; the chip names it and is saved.
    key_on(&mut dom, "Thread thread-b", Key::ContextMenu);
    click_control(&mut dom, "Reference in chat");
    until(&mut dom, "the chip", |dom| control(dom, "Remove thread-b").is_some()).await;
    assert!(rendered_text(&dom).contains("Thread") && saved(state, "thread-a").len() == 1);
    assert_eq!(saved(state, "thread-a")[0]["kind"], "thread");
    // The same thread twice is one chip; a thread cannot be attached to itself.
    key_on(&mut dom, "Thread thread-b", Key::ContextMenu);
    click_control(&mut dom, "Reference in chat");
    assert_eq!(saved(state, "thread-a").len(), 1);
    key_on(&mut dom, "Thread thread-a", Key::ContextMenu);
    assert!(crate::runtime::diff_tests::rendered_text(&dom).contains("Reference in chat"));
    assert!(super::transport_tests::control_attribute(&dom, "Reference in chat", "disabled").and_then(|(_, value)| value).as_deref() == Some("true"));
    key_on(&mut dom, "Thread actions", Key::Escape);

    // Removing it empties the saved list on purpose; adding it again and sending claims it.
    click_control(&mut dom, "Remove thread-b");
    assert!(control(&dom, "Remove thread-b").is_none() && saved(state, "thread-a").is_empty());
    key_on(&mut dom, "Thread thread-b", Key::ContextMenu);
    click_control(&mut dom, "Reference in chat");
    until(&mut dom, "the chip", |dom| control(dom, "Remove thread-b").is_some()).await;
    input_control(&mut dom, "Message", "compare with that one");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the turn", |_| completed_runs(state) == 1).await;
    let records = message_context(state);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!((records[0]["kind"].as_str(), records[0]["threadId"].as_str(), records[0]["environmentId"].as_str()), (Some("thread"), Some("thread-b"), Some("diff-ui")));
    let projection = state.peek().thread.projection.clone().unwrap();
    let text = projection["messages"].as_array().unwrap().iter().find(|message| message["role"] == "user").unwrap()["text"].as_str().unwrap().to_owned();
    assert!(text.starts_with("compare with that one [thread-b](t3-context://v1/thread/thread_thread-b)"), "{text}");
    // The send leaves the draft without them.
    until(&mut dom, "the chips to leave", |dom| control(dom, "Remove thread-b").is_none()).await;
    assert!(saved(state, "thread-a").is_empty());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn chips_saved_by_the_original_app_are_shown_and_sent_and_annotations_are_still_held() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    let original = json!({"version":1,"draftsByThreadKey":{
        "diff-ui:thread-a":{"prompt":"look at this","attachments":[],"terminalContexts":[{"id":"t1","threadId":"thread-a","createdAt":"2026-10-09T10:00:00Z","terminalId":"default","terminalLabel":"Terminal 1","lineStart":2,"lineEnd":4,"text":"error: boom"}]},
        "diff-ui:thread-b":{"prompt":"x","attachments":[],"previewAnnotations":[{"id":"a1"}]}},
        "draftThreadsByThreadKey":{},"logicalProjectDraftThreadKeyByLogicalProjectKey":{}});
    state.peek().draft_storage.document.borrow_mut().source_state = Some(std::rc::Rc::new(original));
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "the chip", |dom| control(dom, "Remove Terminal 1 lines 2-4").is_some()).await;
    assert!(rendered_text(&dom).contains("Terminal"));
    assert!(control(&dom, "Submit message").is_some());
    dom.in_scope(ScopeId::APP, || { *state.draft().write() = "look at this".into(); });
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the turn", |_| completed_runs(state) == 1).await;
    let records = message_context(state);
    assert_eq!((records.len(), records[0]["kind"].as_str(), records[0]["text"].as_str(), records[0]["lineStart"].as_u64()), (1, Some("terminal"), Some("error: boom"), Some(2)));
    // A draft with a page annotation is still held for the original app.
    open_thread(&mut dom, &props, state, "thread-b").await;
    wait_for_rendered_text(&mut dom, "This draft includes saved context.").await;
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_comment_on_a_diff_line_becomes_a_review_chip_sent_with_the_message() {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    turn(&mut dom, &props, state, 1).await;
    fixture.write("hello.txt", "world\n");
    turn(&mut dom, &props, state, 2).await;
    wait_for_rendered_text(&mut dom, "1 changed file").await;
    click_control(&mut dom, "Show diff for hello.txt");
    wait_for_rendered_text(&mut dom, "@@ -0,0 +1 @@").await;
    click_control(&mut dom, "Comment on hello.txt line 1");
    assert!(control(&dom, "Add comment to chat").is_some());
    assert!(super::transport_tests::control_attribute(&dom, "Add comment to chat", "disabled").and_then(|(_, value)| value).as_deref() == Some("true"), "an empty comment says nothing");
    input_control(&mut dom, "Review comment", "Name this better");
    click_control(&mut dom, "Add comment to chat");
    until(&mut dom, "the chip", |dom| control(dom, "Remove hello.txt L1").is_some()).await;
    assert!(control(&dom, "Review comment").is_none(), "the dialog closes");
    let chip = saved(state, "thread-a").remove(0);
    assert_eq!((chip["kind"].as_str(), chip["filePath"].as_str(), chip["text"].as_str(), chip["diff"].as_str()), (Some("review-comment"), Some("hello.txt"), Some("Name this better"), Some("+world")));
    input_control(&mut dom, "Message", "please fix");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the turn", |_| completed_runs(state) == 3).await;
    let records = message_context(state);
    assert_eq!((records.len(), records[0]["kind"].as_str(), records[0]["rangeLabel"].as_str()), (1, Some("review-comment"), Some("+1")));
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_thread_started_in_the_background_leaves_the_person_on_a_fresh_form() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    dom.in_scope(ScopeId::APP, || new_thread(&props.transport, state, Some("project".into())));
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the form", |dom| control(dom, "First message").is_some()).await;
    until(&mut dom, "a model", |dom| !crate::runtime::transport_tests::control_attribute(dom, "Create new thread", "disabled").and_then(|(_, value)| value).as_deref().is_some_and(|value| value == "true")).await;
    assert!(super::transport_tests::control_attribute(&dom, "Start in background", "disabled").and_then(|(_, value)| value).as_deref() == Some("true"), "there is nothing to start yet");
    input_control(&mut dom, "First message", "Look into the flaky test");
    click_control(&mut dom, "Start in background");
    until(&mut dom, "the new thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.len() == 3)).await;
    until(&mut dom, "the form to empty", |_| state.new_thread_drafts().peek().values().all(|text| text.is_empty())).await;
    assert!(state.active_thread().peek().is_none(), "the person stays on the form");
    assert!(control(&dom, "First message").is_some());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn files_named_in_a_message_are_shown_through_signed_urls_or_opened_in_the_editor() {
    let fixture = Fixture::start(false).await;
    // Bytes of a file are never read by the page; the server signs a URL for the exact path.
    fixture.write("pic.png", "not really a picture");
    fixture.write("spec.pdf", "%PDF-1.4");
    fixture.write("hello.txt", "one\ntwo\n");
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    input_control(&mut dom, "Message", "see ![pic](pic.png) and [the spec](spec.pdf) and [the code](hello.txt:2) and [the web](https://example.com/a.pdf) and ![away](../outside.png)");
    dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
    until(&mut dom, "the turn", |_| completed_runs(state) == 1).await;
    until(&mut dom, "the picture", |dom| control(dom, "Image pic").is_some()).await;
    let src = super::transport_tests::control_attribute(&dom, "Image pic", "src").and_then(|(_, value)| value).unwrap();
    assert!(src.starts_with("http://127.0.0.1:") && src.contains("/api/assets/") && !src.contains("/pic.png/") && src.split('/').nth(5).is_some_and(|token| token.contains('.')), "a signed URL, not the path: {src}");
    // An image that climbs out of the workspace is not fetched at all.
    assert!(control(&dom, "Image away").is_none());

    // A PDF opens in the page, in a frame with no privileges, from a signed URL; Escape closes it.
    click_control(&mut dom, "Open spec.pdf");
    until(&mut dom, "the frame", |dom| control(dom, "Preview frame spec.pdf").is_some()).await;
    let frame = super::transport_tests::control_attribute(&dom, "Preview frame spec.pdf", "src").and_then(|(_, value)| value).unwrap();
    assert!(frame.contains("/api/assets/") && !frame.starts_with("file:"), "{frame}");
    key_on(&mut dom, "Preview spec.pdf", Key::Escape);
    assert!(control(&dom, "Preview spec.pdf").is_none());

    // Any other file goes to the editor at its line; with none to open it in, the person is told. The
    // server is made to offer no editor first, so that nothing on this machine is ever launched.
    {
        let mut config = state.typed_config();
        config.write().as_mut().unwrap().available_editors = t3_contracts::ForwardCompatibleArray(vec![]);
    }
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    click_control(&mut dom, "Open hello.txt at line 2");
    wait_for_rendered_text(&mut dom, "No editor to open the file in").await;
    // A web link stays a web link.
    assert!(control(&dom, "Open https://example.com/a.pdf").is_none());
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_pin_moves_up_and_down_among_the_pins_by_writing_fractional_keys() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    for thread in ["thread-a", "thread-b"] {
        dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.pin", json!({"threadId":thread})));
        until(&mut dom, "the pin", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|t| t.id.as_str() == thread && t3_client::sidebar::is_pinned(t)))).await;
    }
    let order = |state: Store<UiModel>| -> Vec<String> {
        let model = state.peek();
        let snapshot = model.shell.snapshot.as_ref().unwrap();
        let project = snapshot.projects[0].id.clone();
        crate::sidebar_rows::rows(&snapshot.threads, &project, chrono::Utc::now()).into_iter().map(|thread| thread.id.to_string()).collect()
    };
    let first = order(state);
    // The top pin cannot go higher; the bottom one moves up by writing keys that put it first.
    key_on(&mut dom, &format!("Thread {}", first[0]), Key::ContextMenu);
    assert!(super::transport_tests::control_attribute(&dom, "Move pin up", "disabled").and_then(|(_, value)| value).as_deref() == Some("true"));
    click_control(&mut dom, "Move pin down");
    until(&mut dom, "the new order", |_| order(state) == vec![first[1].clone(), first[0].clone()]).await;
    let keys: Vec<Option<String>> = state.peek().shell.snapshot.as_ref().unwrap().threads.iter().filter(|t| t3_client::sidebar::is_pinned(t)).map(|t| t.pin_order_key.clone().flatten().map(|key| key.to_string())).collect();
    assert!(keys.iter().all(Option::is_some), "neighbours without keys get them: {keys:?}");
    // Moving back is one write.
    key_on(&mut dom, &format!("Thread {}", first[0]), Key::ContextMenu);
    click_control(&mut dom, "Move pin up");
    until(&mut dom, "the order again", |_| order(state) == first).await;
    fixture.finish(dom).await;
}
