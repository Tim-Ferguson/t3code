//! The thread's Pull requests tab through the mounted UI and the real native server: several links open the
//! tab from the sidebar badge, a link is unlinked from its menu and a new one is linked by URL from the
//! dialog, and the palette offers the same two actions.
use super::diff_tests::{Fixture, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::keys_tests::{PALETTE, open_palette};
use crate::runtime::transport_tests::{click_when_enabled, control, input_control};

fn link(number: u64) -> Value {
    json!({"threadId":"thread-a","host":"github.com","repository":"acme/web","number":number,"url":format!("https://github.com/acme/web/pull/{number}"),"source":"manual"})
}

#[tokio::test(flavor = "current_thread")]
async fn several_links_open_the_tab_from_the_badge_and_are_unlinked_and_linked_from_it() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    for number in [7, 8] {
        dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.pull-request.link", link(number)));
    }
    // Two unrelated links are one badge that counts them; it opens the thread's tab, not one request.
    until(&mut dom, "the badge", |dom| control(dom, "PR #*").is_some()).await;
    assert!(crate::thread_pull_requests_panel::supported(state), "the fixture server keeps several links per thread");
    assert!(control(&dom, "Close pull requests").is_none());
    click_when_enabled(&mut dom, "PR #*").await;
    until(&mut dom, "the tab", |dom| control(dom, "Close pull requests").is_some()).await;
    assert!(state.peek().pr_panel.is_none(), "the tab, not a single pull request's panel");
    wait_for_rendered_text(&mut dom, "2 open").await;
    assert!(rendered_text(&dom).contains("2 linked"));

    // The row's menu unlinks it from the thread.
    click_when_enabled(&mut dom, "Actions for #7").await;
    click_when_enabled(&mut dom, "Unlink from thread").await;
    until(&mut dom, "one link left", |dom| rendered_text(dom).contains("1 linked") && control(dom, "Actions for #7").is_none()).await;

    // The footer's Link opens the dialog; a URL names the request, and the server files it under the thread.
    click_when_enabled(&mut dom, "Link a pull request").await;
    until(&mut dom, "the dialog", |dom| control(dom, "Pull request URL or #42").is_some()).await;
    input_control(&mut dom, "Pull request URL or #42", "https://github.com/acme/web/pull/9");
    wait_for_rendered_text(&mut dom, "github.com/acme/web #9").await;
    click_when_enabled(&mut dom, "Confirm link").await;
    until(&mut dom, "linked", |dom| control(dom, "Pull request URL or #42").is_none() && rendered_text(dom).contains("2 linked") && control(dom, "Actions for #9").is_some()).await;
    // Text that names no request is said so and links nothing.
    click_when_enabled(&mut dom, "Link a pull request").await;
    input_control(&mut dom, "Pull request URL or #42", "not a request");
    wait_for_rendered_text(&mut dom, "Use a pull request URL, 123, or #123.").await;
    fixture.finish(dom).await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_palette_links_a_pull_request_shows_the_tab_and_restarts_the_session() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the config", |_| state.peek().typed_config.is_some()).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    // Nothing is linked: the entry for the tab is there but cannot be chosen.
    open_palette(&mut dom).await;
    assert!(control(&dom, "Show linked pull requests").is_some() && control(&dom, "Link pull request to thread").is_some());
    click_when_enabled(&mut dom, "Link pull request to thread").await;
    until(&mut dom, "the dialog", |dom| control(dom, "Pull request URL or #42").is_some() && control(dom, PALETTE).is_none()).await;
    click_when_enabled(&mut dom, "Cancel").await;
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.pull-request.link", link(7)));
    until(&mut dom, "the link", |dom| control(dom, "PR #*").is_some()).await;
    open_palette(&mut dom).await;
    click_when_enabled(&mut dom, "Show linked pull requests").await;
    until(&mut dom, "the tab", |dom| control(dom, "Close pull requests").is_some()).await;

    // Restarting detaches the thread's provider session and says the next message starts a fresh one.
    open_palette(&mut dom).await;
    click_when_enabled(&mut dom, "Restart agent session").await;
    wait_for_rendered_text(&mut dom, "Agent session will restart").await;
    fixture.finish(dom).await;
}
