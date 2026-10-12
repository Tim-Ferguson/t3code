//! The composer's resting layout (setting "Collapse composer on scroll"): scrolling the conversation of an open thread rests
//! the composer into one line, and the next interaction with it lifts that. The conversation's scroll bridge reports each
//! wheel event (`timeline_scroll`), this file keeps what the two sides need to agree on, and the composer draws the layout.
//! The decisions are `t3_client::composer_rest`.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use t3_client::composer_rest::Layout;

/// What the conversation and the composer of the open thread tell each other. Each field is read on its own, so the wheel
/// reporting a gesture does not repaint the composer.
#[derive(Debug, Clone, PartialEq, Default, Store)]
pub struct Model {
    /// The thread (its timeline owner key) whose composer a scroll rested, until something lifts it.
    pub collapsed_for: Option<String>,
    /// The thread whose composer may be rested now: the setting is on and nothing in the composer needs the room.
    pub eligible_for: Option<String>,
    /// Whether the conversation has more than fits above the composer.
    pub overflows: bool,
    /// How often the composer has been interacted with; a scroll gesture under way when it moves is not allowed to rest it.
    pub lifts: u64,
}

/// The key a thread's conversation and composer share.
pub(crate) fn owner(environment: &str, thread: &str) -> String {
    serde_json::to_string(&(environment, thread)).expect("scoped thread identity")
}

/// Rests the composer of `owner` (a scroll gesture past the threshold).
pub(crate) fn rest(state: Store<UiModel>, owner: &str) {
    let mut collapsed = state.composer_rest().collapsed_for();
    if collapsed.peek().as_deref() != Some(owner) {
        collapsed.set(Some(owner.to_owned()));
    }
}

/// Whether the conversation has more than fits above the composer.
pub(crate) fn set_overflow(state: Store<UiModel>, overflows: bool) {
    let mut field = state.composer_rest().overflows();
    if *field.peek() != overflows {
        field.set(overflows);
    }
}

/// Lifts the resting layout: the person focused the composer, typed, or clicked it (`setIsComposerScrollCollapsed(false)`).
pub(crate) fn lift(state: Store<UiModel>) {
    let model = state.composer_rest();
    let mut lifts = model.lifts();
    let next = *lifts.peek() + 1;
    lifts.set(next);
    let mut collapsed = model.collapsed_for();
    if collapsed.peek().is_some() {
        collapsed.set(None);
    }
}

/// Whether the composer of `owner` may be rested by a wheel event now.
pub(crate) fn eligible(state: Store<UiModel>, owner: &str) -> bool {
    let model = state.composer_rest();
    model.eligible_for().peek().as_deref() == Some(owner) && model.collapsed_for().peek().as_deref() != Some(owner)
}

/// The composer's side: tells the conversation whether it may rest it, and answers whether it is resting.
/// `needs_room` is anything open in the composer (an error, an attachment to look at, an unsent send).
pub(crate) fn use_resting(state: Store<UiModel>, owner: String, multiline: bool, needs_room: bool) -> bool {
    let enabled = state.client_settings().read().composer_collapse_on_scroll;
    let may_rest = enabled && !multiline && !needs_room;
    use_effect(use_reactive((&may_rest, &owner), move |(may_rest, owner)| {
        let wanted = may_rest.then_some(owner);
        let mut field = state.composer_rest().eligible_for();
        if *field.peek() != wanted {
            field.set(wanted);
        }
    }));
    // Another thread's composer, or none, is not left claimed.
    use_drop({
        let owner = owner.clone();
        move || {
            let mut field = state.composer_rest().eligible_for();
            if field.peek().as_deref() == Some(owner.as_str()) {
                field.set(None);
            }
        }
    });
    let model = state.composer_rest();
    Layout {
        existing_thread: true,
        scroll_collapsed: enabled && model.collapsed_for().read().as_deref() == Some(owner.as_str()),
        expanded_chrome: needs_room,
        multiline_prompt: multiline,
        timeline_overflows: *model.overflows().read(),
    }
    .resting()
}
