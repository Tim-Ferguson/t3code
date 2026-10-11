//! The composer's refusal of a prompt that is too long, through the mounted UI and the real native server: nothing is sent,
//! the banner says by how much, follows the prompt, and goes once the prompt fits; then the send goes through.
use super::diff_tests::{Fixture, completed_runs, open_thread, rendered_text, seed, until, wait_for_rendered_text};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{click_control, input_control};

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_prompt_over_the_limit_is_not_sent_and_the_banner_follows_the_prompt_until_it_fits() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    let limit = t3_contracts::PROVIDER_SEND_TURN_MAX_INPUT_CHARS;

    // Typing alone says nothing; the refusal comes with the send.
    input_control(&mut dom, "Message", &"x".repeat(limit + 5));
    assert!(!rendered_text(&dom).contains("character limit"));
    click_control(&mut dom, "Submit message");
    wait_for_rendered_text(&mut dom, "Prompt is 5 characters over the 120,000-character limit. Shorten or split it before sending.").await;
    assert_eq!(completed_runs(state), 0);
    assert!(state.peek().pending_messages.is_empty(), "nothing was sent");

    // From then on the banner follows the prompt.
    input_control(&mut dom, "Message", &"x".repeat(limit + 1));
    wait_for_rendered_text(&mut dom, "Prompt is 1 character over the 120,000-character limit.").await;
    input_control(&mut dom, "Message", "short enough");
    until(&mut dom, "the banner gone", |dom| !rendered_text(dom).contains("character limit")).await;
    until(&mut dom, "the refusal forgotten", |_| state.peek().prompt_length_error.is_none()).await;

    // And the prompt that fits goes.
    click_control(&mut dom, "Submit message");
    until(&mut dom, "the turn", |_| completed_runs(state) == 1).await;
    fixture.finish(dom).await;
}
