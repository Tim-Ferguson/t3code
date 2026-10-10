//! What the person sees when the application cannot continue.
//!
//! Two layers, because the two kinds of failure behave differently:
//!
//! * A **panic** on the web target aborts the WebAssembly module (`panic = "abort"`; there is no
//!   unwinding), so the Dioxus runtime is dead the moment the hook returns. [`install`] therefore
//!   sets a panic hook that writes a plain static screen with direct DOM calls: the message, the
//!   location, and a Reload button. The text goes in as text nodes only, never as HTML.
//! * An **error returned by a component** (and, on the native targets, a panic, which Dioxus
//!   catches there) is handled by the `ErrorBoundary` around the open conversation in `App`:
//!   [`ThreadFailed`] replaces that thread's view and leaves the sidebar usable.

use dioxus::prelude::*;
#[cfg(any(test, target_arch = "wasm32"))]
use std::any::Any;
#[cfg(any(test, target_arch = "wasm32"))]
use std::fmt::Write as _;

/// The words of a failure, ready to show or log.
#[cfg(any(test, target_arch = "wasm32"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub message: String,
    pub location: Option<String>,
}

#[cfg(any(test, target_arch = "wasm32"))]
impl Report {
    /// The message and the place it was raised, as the console line and the screen both show it.
    pub fn text(&self) -> String {
        let mut text = self.message.clone();
        if let Some(location) = &self.location {
            let _ = write!(text, "\n    at {location}");
        }
        text
    }
}

/// The report for a panic payload, which is a `&str` or a `String` unless something panicked with
/// a custom value.
#[cfg(any(test, target_arch = "wasm32"))]
pub(crate) fn describe(payload: &(dyn Any + Send), location: Option<(&str, u32, u32)>) -> Report {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "The application panicked without a message.".to_owned());
    Report {
        message,
        location: location.map(|(file, line, column)| format!("{file}:{line}:{column}")),
    }
}

/// The card that replaces the open conversation when something in it failed. The error is text, shown
/// as such; "Try again" clears it so the conversation renders afresh.
pub(crate) fn thread_failed(errors: ErrorContext) -> Element {
    let message = errors.error().map(|error| error.to_string()).unwrap_or_default();
    rsx! {
        section { class: "empty-state", role: "alert",
            h1 { "Something went wrong." }
            p { "This conversation could not be shown. The rest of the app still works." }
            if !message.is_empty() { p { class: "muted", "{message}" } }
            button { class: "primary", r#type: "button", onclick: move |_| errors.clear_errors(), "Try again" }
        }
    }
}

/// Installs the panic hook of the web target. Safe to call again: only the first call installs. The
/// application calls it before launch and again from the first render, because the Dioxus web
/// runtime sets its own hook when it starts and the later call puts this one back on top.
#[cfg(target_arch = "wasm32")]
pub fn install() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static SHOWN: AtomicBool = AtomicBool::new(false);
    std::panic::set_hook(Box::new(|info| {
        // A panic while the screen is being built must not recurse; one screen is enough.
        if SHOWN.swap(true, Ordering::SeqCst) {
            return;
        }
        let report = describe(
            info.payload(),
            info.location().map(|place| (place.file(), place.line(), place.column())),
        );
        web::show(&report);
    }));
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::Report;
    use wasm_bindgen::{closure::Closure, JsCast, JsValue};

    const SCREEN: &str = "position:fixed;inset:0;z-index:2147483647;overflow:auto;display:flex;align-items:center;\
        justify-content:center;padding:16px;box-sizing:border-box;background:Canvas;color:CanvasText;\
        color-scheme:light dark;font-family:system-ui,-apple-system,'Segoe UI',sans-serif";
    const CARD: &str = "width:100%;max-width:640px";
    const TITLE: &str = "margin:0 0 8px;font-size:20px;font-weight:600";
    const LEAD: &str = "margin:0 0 16px;font-size:14px;line-height:1.5;opacity:.8";
    const DETAILS: &str = "margin:0 0 16px;padding:10px 12px;max-height:50vh;overflow:auto;font:12px/1.5 ui-monospace,\
        SFMono-Regular,Menlo,monospace;white-space:pre-wrap;overflow-wrap:anywhere;border:1px solid GrayText;border-radius:8px";
    const BUTTON: &str = "padding:8px 14px;font:inherit;font-size:14px;cursor:pointer;color:inherit;background:transparent;\
        border:1px solid GrayText;border-radius:8px";

    /// Logs the failure once and replaces the page with the error screen. Every step ignores its
    /// own failure: a panic inside the hook would abort without showing anything.
    pub(super) fn show(report: &Report) {
        let text = report.text();
        let stack = js_sys::Reflect::get(&js_sys::Error::new(""), &JsValue::from_str("stack"))
            .ok()
            .and_then(|stack| stack.as_string())
            .unwrap_or_default();
        web_sys::console::error_1(&JsValue::from_str(&format!("T3 Code panicked: {text}\n{stack}")));
        let _ = build(&text);
    }

    fn build(text: &str) -> Option<()> {
        let document = web_sys::window()?.document()?;
        let body = document.body()?;
        let element = |tag: &str, style: &str, content: &str| {
            let node = document.create_element(tag).ok()?;
            let _ = node.set_attribute("style", style);
            node.set_text_content(Some(content));
            Some(node)
        };
        let screen = element("div", SCREEN, "")?;
        let _ = screen.set_attribute("role", "alert");
        let card = element("div", CARD, "")?;
        let heading = element("h1", TITLE, "Something went wrong.")?;
        let _ = card.append_child(&heading);
        let lead = element("p", LEAD, "T3 Code hit an error and stopped. Reloading the app usually fixes it.")?;
        let _ = card.append_child(&lead);
        let details = element("pre", DETAILS, text)?;
        let _ = card.append_child(&details);
        let reload = element("button", BUTTON, "Reload app")?;
        let _ = reload.set_attribute("type", "button");
        let on_click = Closure::<dyn FnMut()>::new(|| {
            if let Some(window) = web_sys::window() {
                let _ = window.location().reload();
            }
        });
        let _ = reload.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref());
        on_click.forget();
        let _ = card.append_child(&reload);
        let _ = screen.append_child(&card);
        body.set_text_content(None);
        let _ = body.append_child(&screen);
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_payload_becomes_its_message_and_place() {
        let literal = describe(&"index out of bounds", Some(("src/lib.rs", 12, 5)));
        assert_eq!(literal.text(), "index out of bounds\n    at src/lib.rs:12:5");
        let formatted = describe(&String::from("bad value 7"), None);
        assert_eq!(formatted, Report { message: "bad value 7".into(), location: None });
        let other = describe(&42_u32, None);
        assert_eq!(other.message, "The application panicked without a message.");
    }

    /// `Instant::now()`, `SystemTime::now()`, threads and blocking calls compile for the browser and
    /// then abort the whole WebAssembly module. `rust/tools/check_wasm_calls.mjs` finds them in the
    /// crates the web build compiles in; it needs `node`, which the repository's tooling provides.
    #[test]
    fn the_browser_build_calls_nothing_the_target_cannot_run() {
        let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/check_wasm_calls.mjs");
        match std::process::Command::new("node").arg(&script).output() {
            Ok(output) => assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("node is not installed; skipped {}", script.display());
            }
            Err(error) => panic!("could not run node: {error}"),
        }
    }

    #[component]
    fn Boom(panics: bool) -> Element {
        if panics {
            panic!("the conversation blew up");
        }
        Err(dioxus::dioxus_core::CapturedError::from_display("a returned failure").into())
    }

    fn failing_returns() -> Element {
        rsx! { aside { "sidebar still here" } ErrorBoundary { handle_error: thread_failed, Boom { panics: false } } }
    }

    fn failing_panics() -> Element {
        rsx! { aside { "sidebar still here" } ErrorBoundary { handle_error: thread_failed, Boom { panics: true } } }
    }

    /// The native targets: a failure inside the conversation, whether returned or a panic, shows the
    /// card where the conversation was and leaves its siblings (the sidebar) alone.
    #[test]
    fn a_failing_conversation_shows_a_card_and_leaves_the_rest_standing() {
        for app in [failing_returns as fn() -> Element, failing_panics] {
            let mut dom = VirtualDom::new(app);
            dom.rebuild_in_place();
            dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
            let text = crate::runtime::transport_tests::rendered_text(&dom);
            assert!(text.contains("sidebar still here"), "{text}");
            assert!(text.contains("Something went wrong."), "{text}");
            assert!(text.contains("Try again"), "{text}");
        }
    }
}
