//! Error text a server, provider or host wrote: plain text, a few lines tall, wrapped, with a way
//! to read all of it or copy it. A traceback pasted into a toast must not fill the screen; the
//! full text stays one click away, inside a box that scrolls on its own.
use dioxus::prelude::*;

/// Lines shown before the rest is folded away (the stylesheet clamps to the same number).
pub(crate) const CLAMP_LINES: usize = 4;
/// Characters that wrap onto more than the clamped lines in the narrowest place this is shown.
const CLAMP_CHARS: usize = 200;

/// Whether the text may overflow the clamp and so gets the "Show more" and "Copy" controls.
pub(crate) fn needs_clamp(text: &str) -> bool {
    text.lines().count() > CLAMP_LINES || text.chars().count() > CLAMP_CHARS
}

#[component]
pub(crate) fn ErrorText(text: String) -> Element {
    let mut open = use_signal(|| false);
    let long = needs_clamp(&text);
    let expanded = long && open();
    let copy = {
        let text = text.clone();
        move |_| crate::sidebar_rows::copy_text(&text)
    };
    rsx! {
        div { class: "error-text", "data-expanded": "{expanded}",
            p { class: "error-text-body", "{text}" }
            if long {
                div { class: "error-text-tools",
                    button { r#type: "button", "aria-label": if expanded { "Show less" } else { "Show more" }, "aria-expanded": "{expanded}", onclick: move |_| { let next = !open(); open.set(next); }, if expanded { "Show less" } else { "Show more" } }
                    button { r#type: "button", "aria-label": "Copy error text", onclick: copy, "Copy" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_text_that_can_overflow_gets_the_controls() {
        assert!(!needs_clamp("Could not reach the host."));
        assert!(!needs_clamp("one\ntwo\nthree\nfour"));
        assert!(needs_clamp("one\ntwo\nthree\nfour\nfive"));
        assert!(needs_clamp(&"x".repeat(CLAMP_CHARS + 1)));
        assert!(!needs_clamp(&"x".repeat(CLAMP_CHARS)));
    }
}
