//! What a page says when it has nothing to list, and why: no project to ask, a host that could
//! not be read, or a filter that left nothing. Port of the original's `Empty` blocks, drawn in
//! the page's own flow rather than as an illustration.
use dioxus::prelude::*;

/// A centred title and explanation, and the actions that could change the answer.
#[component]
pub(crate) fn EmptyState(title: String, description: String, children: Element) -> Element {
    rsx! {
        div { class: "empty-card", role: "status",
            h2 { "{title}" }
            p { class: "muted", "{description}" }
            div { class: "empty-actions", {children} }
        }
    }
}
