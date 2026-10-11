//! What the settings pages share: the tab row that moves between them, the confirmation dialog a
//! destructive step asks for, and the grants a page's writes need.
use crate::runtime::{UiModel, UiModelStoreExt, View};
use dioxus::prelude::*;
use t3_client::connection::ConnectionStatus;
use t3_contracts::AuthEnvironmentScope;

const TABS: [(View, &str); 10] = [
    (View::General, "General"),
    (View::Projects, "Projects"),
    (View::Providers, "Providers"),
    (View::Appearance, "Appearance"),
    (View::Connections, "Connections"),
    (View::Keybindings, "Keybindings"),
    (View::ScheduledTasks, "Scheduled tasks"),
    (View::SourceControl, "Source control"),
    (View::TextGeneration, "Text generation"),
    (View::Diagnostics, "Diagnostics"),
];

/// The settings page a catalog entry's page key names, where this app has one.
pub(crate) fn page_view(page: &str) -> Option<View> {
    Some(match page {
        "general" => View::General,
        "projects" => View::Projects,
        "appearance" => View::Appearance,
        "providers" => View::Providers,
        "connections" => View::Connections,
        "keybindings" => View::Keybindings,
        "scheduled-tasks" => View::ScheduledTasks,
        "source-control" => View::SourceControl,
        "text-generation" => View::TextGeneration,
        "diagnostics" => View::Diagnostics,
        _ => return None,
    })
}
/// Which gates of the settings catalog hold in this app.
pub(crate) fn search_availability(state: Store<UiModel>) -> t3_client::settings_search::Availability {
    let platform = crate::keys::platform();
    let auto = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_auto_settlement == Some(true));
    t3_client::settings_search::Availability { desktop: crate::keys::desktop(), mac: platform.is_mac(), windows: false, thread_auto_settlement: auto }
}
/// The entries the query finds that this app has a place for.
pub(crate) fn find_settings(state: Store<UiModel>, query: &str) -> Vec<&'static t3_client::settings_search::Item> {
    use t3_client::settings_search as search;
    search::search(query, search::items().iter().filter(|item| matches!(search::placement(&item.id), search::Placement::Here(_))), search_availability(state))
}
/// Goes to the page an entry lives on and brings its row into view.
pub(crate) fn go_to_setting(state: Store<UiModel>, item: &t3_client::settings_search::Item) {
    use t3_client::settings_search as search;
    let search::Placement::Here(page) = search::placement(&item.id) else { return };
    let Some(view) = page_view(page) else { return };
    state.view().set(view);
    let (id, title) = (serde_json::to_string(item.target_id.as_deref().unwrap_or(&item.id)).unwrap_or_default(), serde_json::to_string(&item.title).unwrap_or_default());
    spawn(async move {
        // The page is drawn after the view changes; its row is found once it is there.
        crate::runtime::delay(80).await;
        let _ = document::eval(&format!("const id={id},title={title};const el=document.getElementById(id)||[...document.querySelectorAll('[aria-label]')].find(e=>e.getAttribute('aria-label')===title);el?.scrollIntoView({{block:'center'}});el?.focus?.();"));
    });
}

/// The box above the settings pages: type what a setting is called or does, and pick it.
#[component]
fn Search(state: Store<UiModel>) -> Element {
    let mut query = use_signal(String::new);
    let typed = query.read().clone();
    let hits: Vec<_> = find_settings(state, &typed).into_iter().take(8).collect();
    rsx! { div { class: "settings-search", role: "search",
        input { r#type: "search", "aria-label": "Search settings", placeholder: "Search settings", autocomplete: "off", value: "{typed}", oninput: move |event| query.set(event.value()) }
        if !typed.trim().is_empty() {
            if hits.is_empty() { p { class: "muted", role: "status", "No settings found." } }
            else {
                ul { class: "settings-search-results", "aria-label": "Matching settings",
                    for item in hits { li { key: "{item.id}", button { r#type: "button", "aria-label": "{item.title}", onclick: move |_| { query.set(String::new()); go_to_setting(state, item); },
                        strong { "{item.title}" } span { class: "muted", " Settings \u{b7} {t3_client::settings_search::section_label(&item.to)}" } } } }
                }
            }
        }
    } }
}

/// The row of settings pages, with the current one marked.
#[component]
pub(crate) fn Tabs(state: Store<UiModel>, current: View) -> Element {
    rsx! { Search { state } div { class: "settings-tabs", role: "navigation", "aria-label": "Settings pages",
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
