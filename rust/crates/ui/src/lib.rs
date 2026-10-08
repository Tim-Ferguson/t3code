//! UI entry while the shared client port is validated. This is not a parity-complete client.
use dioxus::prelude::*;

#[component]
pub fn App() -> Element {
    rsx! {
        main {
            h1 { "T3 Code" }
            p { "The Rust UI migration is in progress. The shared client state is implemented; application surfaces are still being ported." }
        }
    }
}
