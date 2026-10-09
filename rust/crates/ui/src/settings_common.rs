//! What the settings pages share: the tab row that moves between them, the confirmation dialog a
//! destructive step asks for, and the grants a page's writes need.
use crate::runtime::{UiModel, UiModelStoreExt, View};
use dioxus::prelude::*;
use t3_client::connection::ConnectionStatus;
use t3_contracts::AuthEnvironmentScope;

const TABS: [(View, &str); 7] = [
    (View::Providers, "Providers"),
    (View::Appearance, "Appearance"),
    (View::Connections, "Connections"),
    (View::Keybindings, "Keybindings"),
    (View::ScheduledTasks, "Scheduled tasks"),
    (View::SourceControl, "Source control"),
    (View::TextGeneration, "Text generation"),
];

/// The row of settings pages, with the current one marked.
#[component]
pub(crate) fn Tabs(state: Store<UiModel>, current: View) -> Element {
    rsx! { div { class: "settings-tabs", role: "navigation", "aria-label": "Settings pages",
        for (view, label) in TABS { button { key: "{label}", class: if view == current { "selected" } else { "" }, "aria-current": if view == current { "page" } else { "false" }, onclick: { let selected = view == current; move |_| if !selected { state.view().set(view.clone()) } }, "{label}" } }
    } }
}

/// Whether the connected environment grants `scope` to this session right now.
pub(crate) fn connected_allows(state: Store<UiModel>, scope: AuthEnvironmentScope) -> bool {
    *state.status().read() == ConnectionStatus::Connected && state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, scope))
}

/// A question a destructive step asks first. Escape or Cancel declines; the dialog takes focus
/// so the key reaches it.
#[component]
pub(crate) fn Confirm(label: String, title: String, body: String, confirm: String, busy: bool, oncancel: EventHandler<()>, onconfirm: EventHandler<()>) -> Element {
    rsx! { div { class: "modal-backdrop", onclick: move |_| oncancel.call(()),
        div { class: "dialog confirm-dialog", role: "alertdialog", "aria-modal": "true", "aria-label": "{title}", "aria-labelledby": "confirm-heading", "aria-describedby": "confirm-body", tabindex: "-1",
            onclick: move |event| event.stop_propagation(),
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); oncancel.call(()); } },
            h2 { id: "confirm-heading", "{title}" }
            p { id: "confirm-body", "{body}" }
            div { class: "dialog-actions",
                button { r#type: "button", "aria-label": "Cancel {label}", onclick: move |_| oncancel.call(()), "Cancel" }
                button { class: "destructive", r#type: "button", "aria-label": "{confirm}", disabled: busy, onclick: move |_| onconfirm.call(()), "{confirm}" }
            }
        }
    } }
}

/// Puts text on the clipboard. The access starts in the click that asked for it.
pub(crate) async fn copy_text(text: &str) -> bool {
    let bytes = serde_json::to_string(text).expect("clipboard text");
    document::eval(&format!("await navigator.clipboard.writeText({bytes});return true;")).join::<bool>().await.unwrap_or(false)
}
