//! The glyph a pull request wears for its state (`PullRequestGlyph`): open, draft, closed, merged and the stack of several.
//! Drawn from the lucide shapes the original uses, in the state's colour (`.pr-glyph.<state>`).
use dioxus::prelude::*;
use t3_client::thread_pull_requests::State;

#[component]
pub(crate) fn Glyph(state: State, stack: bool) -> Element {
    let class = format!("pr-glyph {}", state.class());
    rsx! {
        svg { class: "{class}", "aria-hidden": "true", width: "12", height: "12", view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2", stroke_linecap: "round", stroke_linejoin: "round",
            if stack {
                path { d: "m12.83 2.18a2 2 0 0 0-1.66 0L2.6 6.08a1 1 0 0 0 0 1.83l8.58 3.91a2 2 0 0 0 1.66 0l8.58-3.9a1 1 0 0 0 0-1.83Z" }
                path { d: "m22 17.65-9.17 4.16a2 2 0 0 1-1.66 0L2 17.65" }
                path { d: "m22 12.65-9.17 4.16a2 2 0 0 1-1.66 0L2 12.65" }
            } else {
                match state {
                    State::Open => rsx! { circle { cx: "5", cy: "6", r: "3" } path { d: "M5 9v12" } circle { cx: "19", cy: "18", r: "3" } path { d: "m15 9-3-3 3-3" } path { d: "M12 6h5a2 2 0 0 1 2 2v7" } },
                    State::Draft => rsx! { circle { cx: "18", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" } path { d: "M18 6h.01" } path { d: "M6 9v12" } path { d: "M18 9v3" } },
                    State::Closed => rsx! { circle { cx: "6", cy: "6", r: "3" } path { d: "M6 9v12" } path { d: "m21 3-6 6" } path { d: "m21 9-6-6" } path { d: "M18 11.5V15" } circle { cx: "18", cy: "18", r: "3" } },
                    State::Merged => rsx! { circle { cx: "18", cy: "18", r: "3" } circle { cx: "6", cy: "6", r: "3" } path { d: "M6 21V9a9 9 0 0 0 9 9" } },
                }
            }
        }
    }
}
