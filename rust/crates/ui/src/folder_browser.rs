//! The folders under a path typed in the Add project dialog (`filesystem.browse`, the browse mode of `CommandPalette.tsx`): the server
//! lists the folders of the directory the typed path names, the dialog narrows them to what has been typed of the next name, and a click
//! takes the person one folder further down, or one up. A browser has no folder picker of its own for the server's disk; this is it.
use crate::{
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::folder_browse as logic;
use t3_contracts::AuthEnvironmentScope;

#[component]
pub(crate) fn Entries(state: Store<UiModel>, transport: TransportHandle, mut path: Signal<String>) -> Element {
    let windows = state.typed_config().read().as_ref().is_some_and(|config| serde_json::to_value(config.environment.platform.os).ok().and_then(|os| os.as_str().map(str::to_owned)).as_deref() == Some("windows"));
    let readable = connected_allows(state, AuthEnvironmentScope::FilesystemRead);
    let typed = path.read().clone();
    let browse = logic::browse_of(&typed, windows);
    // The directory is what is asked for; typing the next name only narrows what came back.
    let listed = use_resource({
        let transport = transport.clone();
        move || {
            let transport = transport.clone();
            let directory = logic::browse_of(&path.read(), windows).map(|browse| browse.directory);
            async move {
                let directory = directory?;
                let owner = runtime::response_owner(&transport, state);
                let answer = runtime::request_value_reported(transport.clone(), state, logic::METHOD, json!({"partialPath": directory})).await;
                (owner.is_some() && owner == runtime::response_owner(&transport, state)).then_some((directory, answer))
            }
        }
    });
    let Some(browse) = browse else { return rsx! {} };
    if !readable {
        return rsx! { p { class: "muted", role: "status", "This connection cannot browse host folders." } };
    }
    let shown = listed.read().clone().flatten();
    rsx! {
        div { class: "folder-browser", "aria-label": "Folders on the server",
            if browse.can_go_up {
                if let Some(parent) = browse.parent.clone() { button { r#type: "button", "aria-label": "Up one level", onclick: move |_| path.set(parent.clone()), "\u{2191} Up one level" } }
            }
            match shown {
                // The answer for another directory than the one typed is not shown as this one's.
                Some((directory, answer)) if directory == browse.directory => match answer {
                    Err(message) => rsx! { p { class: "muted", role: "alert", "{message}" } },
                    Ok(result) => {
                        let entries = result["entries"].as_array().cloned().unwrap_or_default();
                        let visible: Vec<Value> = logic::visible_entries(&entries, &browse.filter);
                        if visible.is_empty() { rsx! { p { class: "muted", "No matching folders." } } } else {
                            rsx! { ul { class: "folder-list",
                                for entry in visible {
                                    {
                                        let name = entry["name"].as_str().unwrap_or_default().to_owned();
                                        rsx! { li { key: "{name}", button { r#type: "button", "aria-label": "Open folder {name}", onclick: { let name = name.clone(); move |_| { let next = logic::append_segment(&path.peek(), &name); path.set(next); } }, "{name}" } } }
                                    }
                                }
                            } }
                        }
                    }
                },
                _ => rsx! { p { class: "muted", role: "status", "Looking for folders\u{2026}" } },
            }
        }
    }
}
