//! The comparison target of the "Changes" diff: "Automatic" (the server picks the base) or a
//! branch, with the remote twin of a branch one switch away. Port of the base-ref combobox of
//! `DiffPanel.tsx`. The refs are read fresh each time the picker opens and again when the search
//! settles; nothing is held while it is closed.
use crate::runtime::{self, TransportHandle, UiModel};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{
    review::{self, BaseChoice},
    vcs::{self, Refs},
};

#[derive(Debug, Clone, PartialEq, Default)]
struct Listing {
    local: Refs,
    remote: Refs,
    loading: bool,
    error: Option<String>,
}

#[component]
pub(crate) fn Picker(
    state: Store<UiModel>,
    transport: TransportHandle,
    cwd: String,
    /// The branch being compared, which is not a base for itself.
    head: Option<String>,
    /// The base the reader chose, or `None` for Automatic.
    selected: Option<String>,
    on_pick: Callback<Option<String>>,
    on_close: Callback<()>,
) -> Element {
    let mut query = use_signal(String::new);
    let mut settled = use_signal(String::new);
    let mut listing = use_signal(|| Listing { loading: true, ..Listing::default() });

    // The server is asked once typing pauses.
    use_resource(move || async move {
        let typed = query.read().trim().to_owned();
        if typed != *settled.peek() {
            runtime::delay(vcs::QUERY_DEBOUNCE_MS).await;
            settled.set(typed);
        }
    });
    {
        let (transport, cwd) = (transport.clone(), cwd.clone());
        use_resource(move || {
            let (transport, cwd, search) = (transport.clone(), cwd.clone(), settled());
            async move {
                listing.write().loading = true;
                let owner = runtime::response_owner(&transport, state);
                let mut next = Listing::default();
                for kind in ["local", "remote"] {
                    let mut payload = json!({"cwd":cwd,"includeMatchingRemoteRefs":true,"refKind":kind,"limit":vcs::REF_PAGE_LIMIT});
                    if !search.is_empty() {
                        payload["query"] = json!(search);
                    }
                    let answer = runtime::request_value_reported(transport.clone(), state, vcs::LIST_REFS, payload).await;
                    if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                        return;
                    }
                    let refs = if kind == "local" { &mut next.local } else { &mut next.remote };
                    if let Err(error) = answer.and_then(|page| refs.merge(&page)) {
                        next.error = Some(error);
                    }
                }
                *listing.write() = next;
            }
        });
    }

    let snapshot = listing.read().clone();
    let typed = query.read().clone();
    let current = selected.clone();
    let local: Vec<_> = snapshot.local.refs.iter().filter(|candidate| Some(&candidate.name) != head.as_ref()).cloned().collect();
    let choices: Vec<BaseChoice> = review::filter_choices(&review::base_choices(&local, &snapshot.remote.refs), &typed);
    let automatic = typed.trim().is_empty();
    rsx! {
        div { class: "branch-picker-backdrop", onclick: move |_| on_close.call(()) }
        div { class: "branch-picker base-picker", role: "dialog", "aria-label": "Comparison target",
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); on_close.call(()); } },
            input { class: "branch-search", "aria-label": "Search refs", placeholder: "Search refs...", autocomplete: "off", value: "{typed}",
                onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
                oninput: move |event| query.set(event.value()) }
            div { class: "branch-list", role: "listbox", "aria-label": "Comparison targets",
                if automatic {
                    button { class: "branch-row", role: "option", "aria-selected": "{current.is_none()}", "aria-label": "Automatic", onclick: move |_| on_pick.call(None),
                        span { class: "branch-row-name", "Automatic" } }
                }
                for choice in choices {
                    {
                        let value = choice.value(current.as_deref()).to_owned();
                        let on_remote = choice.remote.as_deref() == Some(value.as_str());
                        let marked = current.as_deref() == Some(value.as_str());
                        let label = choice.label.clone();
                        rsx! {
                            div { key: "{label}", class: "base-choice",
                                button { class: "branch-row", role: "option", "aria-selected": "{marked}", "aria-label": "{label}", onclick: { let value = value.clone(); move |_| on_pick.call(Some(value.clone())) },
                                    span { class: "branch-row-name", "{label}" }
                                    if choice.local.is_none() { span { class: "branch-row-badge", title: "Remote only", "remote" } }
                                }
                                if choice.local.is_some() && choice.remote.is_some() {
                                    button { class: "branch-action", role: "switch", "aria-checked": "{on_remote}", "aria-label": "Use remote version of {label}", title: "Use the remote version",
                                        onclick: { let (local, remote) = (choice.local.clone(), choice.remote.clone()); move |_| on_pick.call(if on_remote { local.clone() } else { remote.clone() }) },
                                        if on_remote { "remote \u{2713}" } else { "remote" } }
                                }
                            }
                        }
                    }
                }
                if !snapshot.loading && snapshot.error.is_none() && choices_empty(&snapshot, &typed) { p { class: "muted branch-empty", "No matching refs." } }
            }
            if let Some(error) = snapshot.error { p { class: "branch-picker-error", role: "alert", "{error}" } }
            if snapshot.loading { p { class: "muted branch-status", role: "status", "Loading refs..." } }
        }
    }
}

fn choices_empty(listing: &Listing, typed: &str) -> bool {
    review::filter_choices(&review::base_choices(&listing.local.refs, &listing.remote.refs), typed).is_empty() && !typed.trim().is_empty()
}
