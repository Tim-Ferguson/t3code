//! The thread details card through the mounted application and the real native server with a temporary git repository: where it
//! is (inline beside the conversation, or a popover from the header), that its open state is saved, which controls it hosts in place of
//! the header, and how its content folds when the room is short. The page's sizes are told to the card as the page's script would
//! tell them (`thread_details::set_container`); the script itself runs only in a page.
use super::diff_tests::{Fixture, Harness, git, open_thread, rendered_text, seed, until};
use super::keys_tests::key_on;
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{all_controls, click_control, control, control_attribute};
use t3_client::thread_panel::{Heights, Presentation};

const OPERATOR: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SourceControlWrite];

async fn with_thread(body: impl AsyncFnOnce(&Fixture, &mut VirtualDom, &Harness, Store<UiModel>)) {
    let fixture = Fixture::start(true).await;
    let (mut dom, props) = fixture.client(Gate::default(), &OPERATOR);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    git(&fixture.workspace, &["branch", "-m", "main"]);
    open_thread(&mut dom, &props, state, "thread-a").await;
    until(&mut dom, "the branch control", |dom| control(dom, "Switch branch").is_some()).await;
    body(&fixture, &mut dom, &props, state).await;
    fixture.finish(dom).await;
}

fn pressed(dom: &VirtualDom) -> bool {
    control_attribute(dom, "Toggle thread details panel", "aria-pressed").and_then(|(_, value)| value).as_deref() == Some("true")
}
/// How the card on screen is presented, when it is.
fn shown_as(dom: &VirtualDom) -> Option<String> {
    control_attribute(dom, "Thread details", "data-thread-details-panel").and_then(|(_, value)| value)
}
fn density(dom: &VirtualDom) -> Option<String> {
    control_attribute(dom, "Thread details", "data-density").and_then(|(_, value)| value)
}
/// Whether the open thread's card is open as a popover, in the saved state itself.
fn popover_open(state: Store<UiModel>) -> bool {
    let key = format!("{}:thread-a", state.peek().destination.clone().unwrap());
    state.peek().right_panel.panels.visibilities.is_open(&key, Presentation::Popover)
}
fn saved(state: Store<UiModel>) -> serde_json::Value {
    serde_json::from_str::<serde_json::Value>(&state.peek().right_panel.panels.to_json()).unwrap()["threadPanelVisibilityByThreadKey"].clone()
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_page_of_unknown_size_gets_a_popover_that_starts_closed_hosts_the_headers_controls_and_closes_on_escape() {
    with_thread(async |_fixture, dom, _props, state| {
        // Closed, the header has the controls: one branch control, none in a card.
        assert!(shown_as(dom).is_none() && !pressed(dom));
        assert_eq!(all_controls(dom, "Switch branch").len(), 1);
        assert!(control(dom, "Choose editor").is_some());

        click_control(dom, "Toggle thread details panel");
        assert_eq!(shown_as(dom).as_deref(), Some("popover"));
        assert!(pressed(dom));
        // Open, the card has them and the header does not: still one of each (the card's branch control reads the workspace afresh).
        until(dom, "the card's controls", |dom| all_controls(dom, "Switch branch").len() == 1 && all_controls(dom, "Choose editor").len() == 1).await;
        assert!(control(dom, "Controls").is_some(), "they are in the card's section");
        assert!(rendered_text(dom).contains("Environment"), "a full card says where the thread works");

        // Escape closes it, and the controls are the header's again.
        key_on(dom, "Thread details", Key::Escape);
        assert!(shown_as(dom).is_none() && !pressed(dom));
        assert!(control(dom, "Controls").is_none());
        until(dom, "the header's controls", |dom| all_controls(dom, "Switch branch").len() == 1).await;
        // A popover is never saved.
        assert_eq!(saved(state), serde_json::json!({}));
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn where_there_is_room_the_card_is_inline_and_open_until_closed_and_closing_it_is_saved() {
    with_thread(async |_fixture, dom, _props, state| {
        crate::thread_details::set_container(state, 1700., 900.);
        until(dom, "the inline card", |dom| shown_as(dom).as_deref() == Some("inline")).await;
        assert!(pressed(dom), "open by default, with no click");
        assert_eq!(control_attribute(dom, "Thread details", "style").and_then(|(_, value)| value).as_deref(), Some("left:1408px;top:60px;width:280px;max-height:876px;right:auto;"));
        until(dom, "the card's branch control", |dom| all_controls(dom, "Switch branch").len() == 1).await;
        assert_eq!(saved(state), serde_json::json!({}), "the default is not saved");

        // The toggle closes this presentation only, and that is what is kept for the thread.
        click_control(dom, "Toggle thread details panel");
        assert!(shown_as(dom).is_none() && !pressed(dom));
        until(dom, "the header's branch control", |dom| all_controls(dom, "Switch branch").len() == 1).await;
        let key = format!("{}:thread-a", state.peek().destination.clone().unwrap());
        assert_eq!(saved(state), serde_json::json!({key.clone(): {"inlineOpen": false, "popoverOpen": false}}));
        // `threadPanel.toggle` opens it again, and the default is kept as nothing.
        crate::thread_details::toggle(state);
        until(dom, "the card again", |dom| shown_as(dom).as_deref() == Some("inline")).await;
        assert_eq!(saved(state), serde_json::json!({}));
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_card_closed_for_one_thread_stays_closed_there_and_a_narrower_page_makes_a_popover_of_it() {
    with_thread(async |_fixture, dom, props, state| {
        crate::thread_details::set_container(state, 1700., 900.);
        until(dom, "the inline card", |dom| shown_as(dom).as_deref() == Some("inline")).await;
        click_control(dom, "Toggle thread details panel");
        assert!(shown_as(dom).is_none());
        // Another thread has the default; this one is still closed on coming back.
        open_thread(dom, props, state, "thread-b").await;
        until(dom, "thread-b's own card", |dom| shown_as(dom).as_deref() == Some("inline")).await;
        open_thread(dom, props, state, "thread-a").await;
        until(dom, "thread-a's card, still closed", |dom| shown_as(dom).is_none() && !pressed(dom)).await;

        // Too narrow for a readable lane beside it: a popover, and the toggle opens that.
        crate::thread_details::set_container(state, 900., 900.);
        click_control(dom, "Toggle thread details panel");
        assert_eq!(shown_as(dom).as_deref(), Some("popover"));
        // Room returning closes the popover: the card is inline-closed for this thread, so nothing shows.
        crate::thread_details::set_container(state, 1700., 900.);
        until(dom, "the popover closed", |_| !popover_open(state)).await;
        assert!(shown_as(dom).is_none());
        crate::thread_details::toggle(state);
        until(dom, "inline again", |dom| shown_as(dom).as_deref() == Some("inline")).await;
        // The edge of room: 1416 is the narrowest the centred 768 lane leaves the card beside it; one less is a popover.
        crate::thread_details::set_container(state, 1415., 900.);
        until(dom, "the popover", |dom| shown_as(dom).is_none()).await;
        assert_eq!(crate::thread_details::presentation(state), Presentation::Popover);
        crate::thread_details::set_container(state, 1416., 900.);
        assert_eq!(crate::thread_details::presentation(state), Presentation::Inline);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_content_folds_to_what_fits_and_unfolds_when_room_returns() {
    with_thread(async |_fixture, dom, _props, state| {
        crate::thread_details::set_container(state, 1700., 900.);
        until(dom, "the inline card", |dom| shown_as(dom).as_deref() == Some("inline")).await;
        // Never measured: shown in full, to be measured.
        assert_eq!(density(dom).as_deref(), Some("full"));
        assert!(rendered_text(dom).contains("Environment") && control(dom, "Choose editor").is_some());

        // The room (876) holds the compact content but not the full one.
        crate::thread_details::set_measured(state, "thread-a", Presentation::Inline, Heights { full: 1200., compact: 400. });
        until(dom, "compact", |dom| density(dom).as_deref() == Some("compact")).await;
        assert!(!rendered_text(dom).contains("Environment"), "the workspace summary folds first");
        until(dom, "the controls", |dom| control(dom, "Choose editor").is_some() && control(dom, "Switch branch").is_some()).await;

        // Not even that: the editor picker folds too, and the branch stays.
        crate::thread_details::set_measured(state, "thread-a", Presentation::Inline, Heights { full: 1200., compact: 1000. });
        until(dom, "essential", |dom| density(dom).as_deref() == Some("essential")).await;
        until(dom, "the branch alone", |dom| control(dom, "Choose editor").is_none() && control(dom, "Switch branch").is_some()).await;

        // Room returns (the window grows): everything is back.
        crate::thread_details::set_container(state, 1700., 1400.);
        until(dom, "full again", |dom| density(dom).as_deref() == Some("full")).await;
        until(dom, "everything", |dom| rendered_text(dom).contains("Environment") && control(dom, "Choose editor").is_some()).await;
    })
    .await;
}
