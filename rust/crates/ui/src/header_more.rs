//! The thread header's secondary actions (rewrite the title, archive, delete) as one group. Where the
//! header has room the buttons sit in its row, as they always did; where it is short (a narrow
//! window, or a side panel open beside the conversation) the stylesheet folds them behind one
//! "More actions" button, as the original's header does. The buttons are the same elements in both
//! shapes, so nothing is drawn twice and a click on one still reaches its own handler.
use dioxus::prelude::*;

#[component]
pub(crate) fn Group(children: Element) -> Element {
    let mut open = use_signal(|| false);
    rsx! {
        div { class: "header-more-group", "data-open": "{open()}",
            onkeydown: move |event| { if open() && event.key() == Key::Escape { event.stop_propagation(); open.set(false); } },
            button { class: "icon-button header-more", r#type: "button", "aria-label": "More actions", "aria-haspopup": "true", "aria-expanded": "{open()}", onclick: move |_| { let next = !open(); open.set(next); }, "\u{22ef}" }
            if open() { div { class: "branch-picker-backdrop header-more-backdrop", onclick: move |_| open.set(false) } }
            div { class: "header-secondary", role: "group", "aria-label": "Thread actions", onclick: move |_| open.set(false), {children} }
        }
    }
}
