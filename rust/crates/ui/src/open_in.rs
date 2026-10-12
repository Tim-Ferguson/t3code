//! "Open in <editor>": the workspace header's split button and the `editor.openFavorite` command.
//! Port of `OpenInPicker.tsx` and `OpenInPickerShortcut.ts` for a server that runs the editor
//! itself (`shell.openInEditor`). The editor chosen last is remembered in this browser, as the
//! original remembers it, and is the one the main button and the shortcut use while the server
//! still offers it. In the desktop app, an environment on another machine is opened by a deep link for the
//! editor on this computer (`desktop_open_in`).
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    settings_common::connected_allows,
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::editors;
use t3_contracts::AuthEnvironmentScope;

/// The editors the connected server offers, as ids.
pub(crate) fn available(state: Store<UiModel>) -> Vec<String> {
    // The desktop app, on an environment of another machine: the editors this computer has, opened by a link.
    #[cfg(any(feature = "desktop", test))]
    if let Some(remote) = crate::desktop_open_in::editors(state) {
        return remote;
    }
    state.typed_config().read().as_ref().map(|config| config.available_editors.0.iter().filter_map(|id| serde_json::to_value(id).ok()?.as_str().map(str::to_owned)).collect()).unwrap_or_default()
}

async fn read_last() -> Option<String> {
    let key = serde_json::to_string(editors::LAST_EDITOR_KEY).expect("key");
    let stored = dioxus::document::eval(&format!("try{{return window.localStorage.getItem({key});}}catch{{return null;}}")).join::<Option<String>>().await.ok().flatten()?;
    // Stored as JSON, as the original's schema-checked storage writes it.
    serde_json::from_str::<String>(&stored).ok()
}
fn write_last(editor: &str) {
    let key = serde_json::to_string(editors::LAST_EDITOR_KEY).expect("key");
    let value = serde_json::to_string(&serde_json::to_string(editor).expect("editor")).expect("value");
    dioxus::document::eval(&format!("try{{window.localStorage.setItem({key},{value});}}catch{{}}"));
}

/// The last editor chosen in this browser, read once when the app starts using it.
#[derive(Clone, Copy)]
pub(crate) struct Last(pub Signal<Option<String>>);
pub(crate) fn use_last() -> Last {
    let mut last = use_signal(|| None::<String>);
    use_hook(move || spawn(async move { last.set(read_last().await) }));
    Last(last)
}

/// Opens `cwd` in `editor` and remembers the choice. One request per click; the answer is reported.
pub(crate) fn open(transport: &TransportHandle, state: Store<UiModel>, mut busy: Signal<bool>, mut last: Signal<Option<String>>, cwd: String, editor: String, reveal: bool) {
    if *busy.peek() {
        return;
    }
    // On another machine the editor here is given a link; no request is made of the server.
    #[cfg(any(feature = "desktop", test))]
    if let Some(opened) = crate::desktop_open_in::open(state, &cwd, &editor) {
        if opened {
            write_last(&editor);
            last.set(Some(editor));
        }
        return;
    }
    if !connected_allows(state, AuthEnvironmentScope::OrchestrationOperate) {
        return;
    }
    busy.set(true);
    write_last(&editor);
    last.set(Some(editor.clone()));
    let transport = transport.clone();
    spawn(async move {
        let mut payload = json!({"cwd":cwd,"editor":editor});
        if reveal {
            payload["reveal"] = json!(true);
        }
        let owner = runtime::response_owner(&transport, state);
        let answer = runtime::request_value_reported(transport.clone(), state, "shell.openInEditor", payload).await;
        if owner.is_some() && owner == runtime::response_owner(&transport, state) {
            if let Err(message) = answer {
                notice::push(state, Level::Error, "Could not open the editor", message);
            }
        }
        busy.set(false);
    });
}

/// The folder the open thread works in: its worktree, else its project's root.
pub(crate) fn thread_cwd(state: Store<UiModel>) -> Option<String> {
    let id = state.active_thread().read().clone()?;
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    let thread = snapshot.threads.iter().find(|thread| thread.id.as_str() == id)?;
    thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| snapshot.projects.iter().find(|project| project.id == thread.project_id).map(|project| project.workspace_root.to_string())).filter(|cwd| !cwd.is_empty())
}

/// Whether an editor may be opened: by a link when the environment is on another machine, else as the server asks.
fn may_open(state: Store<UiModel>) -> bool {
    #[cfg(any(feature = "desktop", test))]
    if crate::desktop_open_in::links(state) {
        return true;
    }
    connected_allows(state, AuthEnvironmentScope::OrchestrationOperate)
}

/// The editor `editor.openFavorite` opens, if the server offers any.
pub(crate) fn favorite(state: Store<UiModel>, last: Option<&str>) -> Option<String> {
    editors::preferred(&available(state), last)
}

#[component]
pub(crate) fn OpenIn(state: Store<UiModel>, transport: TransportHandle, cwd: String) -> Element {
    let last = try_consume_context::<Last>().map_or_else(|| use_signal(|| None::<String>), |last| last.0);
    let busy = use_signal(|| false);
    let mut menu = use_signal(|| false);
    #[cfg(any(feature = "desktop", test))]
    crate::desktop_open_in::use_probe();
    let operable = use_memo(move || may_open(state));
    let offered = available(state);
    let Some(preferred) = editors::preferred(&offered, last.read().as_deref()) else {
        // A machine no link reaches: the button says so, as the original's disabled entry does.
        #[cfg(any(feature = "desktop", test))]
        if let Some(note) = crate::desktop_open_in::no_route(state) {
            return rsx! { div {class:"open-in", button {class:"icon-button","aria-label":"Open in editor",title:"{note}",disabled:true,"Open"}} };
        }
        return rsx! {};
    };
    let label = editors::menu_label(Some(&preferred));
    let choices: Vec<(String, String)> = editors::offered(&offered).into_iter().map(|editor| (editor.id.clone(), editors::entry_label(editor))).collect();
    let title = if operable() { "Open this thread's folder in an editor on the server's machine".to_owned() } else { "This connection cannot open editors.".to_owned() };
    rsx! {
        div {class:"open-in",onkeydown:move |event| {if event.key()==Key::Escape && menu() {event.stop_propagation();menu.set(false);}},
            button {class:"icon-button","aria-label":"{label}",title:"{title}",disabled:!operable() || busy(),
                onclick:{let (transport, cwd, preferred) = (transport.clone(), cwd.clone(), preferred.clone());move |_| open(&transport,state,busy,last,cwd.clone(),preferred.clone(),false)},
                "{preferred_short(&choices, &preferred)}"}
            button {class:"icon-button","aria-label":"Choose editor","aria-haspopup":"menu","aria-expanded":"{menu()}",disabled:!operable(),onclick:move |_| menu.set(!menu()),"\u{25be}"}
            if menu() {div {class:"menu",role:"menu",
                for (id, entry) in choices {
                    button {key:"{id}",role:"menuitem","aria-label":"{entry}",
                        onclick:{let (transport, cwd) = (transport.clone(), cwd.clone());move |_| {menu.set(false);open(&transport,state,busy,last,cwd.clone(),id.clone(),false)}},"{entry}"}
                }
                if let Some(note) = remote_hint(state) { button {role:"menuitem",disabled:true,"{note}"} }
            }}
        }
    }
}
/// The note that stays in the menu until the first open over SSH.
fn remote_hint(state: Store<UiModel>) -> Option<String> {
    #[cfg(any(feature = "desktop", test))]
    return crate::desktop_open_in::hint(state);
    #[cfg(not(any(feature = "desktop", test)))]
    {
        let _ = state;
        None
    }
}
fn preferred_short(choices: &[(String, String)], id: &str) -> String {
    choices.iter().find(|(known, _)| known == id).map_or_else(|| "Open".to_owned(), |(_, label)| if id == editors::FILE_MANAGER { "Open".to_owned() } else { label.clone() })
}
