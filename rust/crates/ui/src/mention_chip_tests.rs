//! The files a message names, through the mounted application and the real native server with a temporary git repository: choosing
//! an `@` path writes the canonical file link into the prompt, each link in the text is a chip above the box that takes it out again,
//! and the file link in a sent message is a chip too. The words that make a mention are `t3_client::composer_mentions`.
use super::diff_tests::{Fixture, Harness, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::keys_tests::key_on;
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, control, input_control};

const OPERATOR: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::FilesystemRead];

async fn with_app(body: impl AsyncFnOnce(&mut VirtualDom, &Harness, Store<UiModel>)) {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.client(Gate::default(), &OPERATOR);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    // The new-thread form's choices exist once the server's configuration has arrived.
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    body(&mut dom, &props, state).await;
    fixture.finish(dom).await;
}

fn draft(state: Store<UiModel>) -> String {
    state.draft().peek().clone()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn choosing_a_path_writes_the_file_link_and_its_chip_takes_the_link_out_again() {
    with_app(async |dom, props, state| {
        open_thread(dom, props, state, "thread-a").await;
        input_control(dom, "Message", "see @READ");
        wait_for_rendered_text(dom, "README.md").await;
        key_on(dom, "Message", Key::Enter);
        // The original writes the canonical link and a space, not the `@path` that was typed.
        until(dom, "the link", |_| draft(state) == "see [README.md](README.md) ").await;
        assert!(control(dom, "Remove README.md").is_some(), "the file is a chip above the box");
        assert!(rendered_text(dom).contains('\u{1F4C4}'), "with the file's icon");

        // The chip takes the link and the space after it out of the text.
        click_control(dom, "Remove README.md");
        assert_eq!(draft(state), "see ");
        assert!(control(dom, "Remove README.md").is_none());

        // A link typed by hand is a chip, and so are quoted paths with spaces; half-typed links and other labels are prose.
        input_control(dom, "Message", "a [x y.rs](dir/x%20y.rs) b @\"my dir/z.rs\" c [the app](src/app.rs) d [q.rs](q.rs)");
        assert!(control(dom, "Remove x y.rs").is_some() && control(dom, "Remove z.rs").is_some());
        assert!(control(dom, "Remove app.rs").is_none() && control(dom, "Remove q.rs").is_none());
        click_control(dom, "Remove z.rs");
        assert_eq!(draft(state), "a [x y.rs](dir/x%20y.rs) b c [the app](src/app.rs) d [q.rs](q.rs)");
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_file_link_in_a_sent_message_is_a_chip_with_the_files_icon_that_opens_it() {
    with_app(async |dom, props, state| {
        open_thread(dom, props, state, "thread-a").await;
        input_control(dom, "Message", "look at [README.md](README.md) please");
        dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
        until(dom, "the sent message", |dom| control(dom, "Open README.md").is_some() && rendered_text(dom).contains("look at")).await;
        assert!(rendered_text(dom).contains('\u{1F4C4}'));
        assert_eq!(state.peek().error, None);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_new_thread_form_draws_the_files_its_message_names_as_chips_too() {
    with_app(async |dom, _props, state| {
        input_control(dom, "First message", "start with [lib.rs](src/lib.rs) now");
        assert!(control(dom, "Remove lib.rs").is_some());
        click_control(dom, "Remove lib.rs");
        let key = (state.peek().destination.clone().unwrap(), state.peek().active_draft.clone().expect("the form shows a draft"));
        assert_eq!(state.peek().new_thread_drafts.get(&key).map(String::as_str), Some("start with now"));
        assert!(control(dom, "Remove lib.rs").is_none());
    })
    .await;
}
