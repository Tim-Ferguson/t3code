//! Prompt recall in the composer, through the mounted UI and the real native server with the scripted fake
//! Codex: ArrowUp on an empty box brings back what was sent, ArrowDown walks forward again, and a box that
//! holds a draft is left to its caret. Waits are on text only the awaited reply produces.
use super::diff_tests::{Fixture, open_thread, seed, until, wait_for_rendered_text};
use super::keys_tests::key_on;
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::input_control;

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn arrow_up_in_an_empty_composer_recalls_the_prompts_sent_and_arrow_down_walks_back() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    open_thread(&mut dom, &props, state, "thread-a").await;
    for prompt in ["First question", "Second question"] {
        input_control(&mut dom, "Message", prompt);
        dom.in_scope(ScopeId::APP, || send_message(&props.transport, state));
        wait_for_rendered_text(&mut dom, &format!("Fixture response: {prompt}")).await;
        until(&mut dom, "the box to empty", |_| state.peek().draft.is_empty()).await;
    }
    key_on(&mut dom, "Message", Key::ArrowUp);
    assert_eq!(state.peek().draft, "Second question");
    key_on(&mut dom, "Message", Key::ArrowUp);
    assert_eq!(state.peek().draft, "First question");
    key_on(&mut dom, "Message", Key::ArrowUp);
    assert_eq!(state.peek().draft, "First question", "the oldest is the end");
    key_on(&mut dom, "Message", Key::ArrowDown);
    assert_eq!(state.peek().draft, "Second question");
    key_on(&mut dom, "Message", Key::ArrowDown);
    assert_eq!(state.peek().draft, "", "past the newest the box is empty again");
    // A draft of the person's own is the caret's: nothing is recalled over it.
    input_control(&mut dom, "Message", "half a thought");
    key_on(&mut dom, "Message", Key::ArrowUp);
    assert_eq!(state.peek().draft, "half a thought");
    fixture.finish(dom).await;
}
