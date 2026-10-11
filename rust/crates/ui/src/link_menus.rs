//! The menus of links and of the files a message names, shown by a right-click: what a link offers (`showExternalLinkContextMenu`),
//! what a pull request's number offers (`showPullRequestLinkContextMenu`), and what a file named in a message offers (the file
//! chip's menu of `ChatMarkdown.tsx`). The items are `t3_client::context_menu`; this carries out the choice.
use crate::{
    notice::{self, Level},
    pull_request_links,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
    sidebar_rows::copy_text,
};
use dioxus::prelude::*;
use t3_client::{
    context_menu::{self as menu, ThreadLink},
    pull_request::Reference,
    thread_pull_requests as links,
};

/// Hands a web page to the system's browser: the desktop app asks the system, the page opens a tab.
pub(crate) fn open_external(url: &str) {
    #[cfg(feature = "desktop")]
    crate::desktop_shell::open_external(url);
    #[cfg(not(feature = "desktop"))]
    {
        let url = serde_json::to_string(url).unwrap_or_default();
        let _ = document::eval(&format!("window.open({url},'_blank','noopener');"));
    }
}

/// Whether a link is a web page the menu is for (`resolveExternalWebLinkHost`): only `http` and `https`, with a host.
pub(crate) fn web_link(href: &str) -> Option<String> {
    let parsed = url::Url::parse(&if href.starts_with("//") { format!("https:{href}") } else { href.to_owned() }).ok()?;
    (matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some_and(|host| !host.is_empty())).then(|| parsed.to_string())
}

/// Whether the thread can be told that a pull request belongs to it, and which way: "Link to thread" for one it does not hold,
/// "Unlink from thread" for one it does. None where linking is unsupported or not allowed, or the link names no request.
fn thread_link(state: Store<UiModel>, thread_id: &str, href: &str) -> Option<(ThreadLink, Reference)> {
    let supported = state.typed_config().read().as_ref().is_some_and(|config| config.environment.capabilities.thread_pull_request_linking == Some(true));
    if !supported || !crate::settings_common::connected_allows(state, t3_contracts::AuthEnvironmentScope::OrchestrationOperate) {
        return None;
    }
    let named = links::parse_change_request_url(href)?;
    let thread = state.shell().read().snapshot.as_ref()?.threads.iter().find(|thread| thread.id.as_str() == thread_id).cloned()?;
    let reference = Reference { project_id: thread.project_id.to_string(), host: Some(named.host), repository: named.repository, number: named.number };
    let value = serde_json::to_value(&thread).ok()?;
    Some((if links::linked_here(&value, &reference) { ThreadLink::Unlink } else { ThreadLink::Link }, reference))
}

/// The menu of a link in a message at the pointer. The integrated browser is not part of this app, so the menu offers the system's
/// browser and the copy; inside a thread it also offers to file a pull request the link names under the thread, or take it out.
pub(crate) async fn external_link(state: Store<UiModel>, transport: Option<TransportHandle>, thread_id: Option<String>, href: String, at: (f64, f64)) {
    let Some(url) = web_link(&href) else { return };
    let target = thread_id.as_deref().and_then(|thread| thread_link(state, thread, &url));
    let items = menu::external_link(false, target.as_ref().map(|(direction, _)| *direction));
    let Some(choice) = crate::context_menu::show("Link actions", items, Some(at)).await else { return };
    match choice.as_str() {
        "open-external" => open_external(&url),
        "copy-link" => copy_text(&url),
        "link-to-thread" | "unlink-from-thread" => {
            if let (Some(transport), Some(thread_id), Some((_, reference))) = (transport, thread_id, target) {
                pull_request_links::update(state, &transport, &thread_id, &reference, &url, choice == "link-to-thread");
            }
        }
        _ => {}
    }
}

/// The right-click on a change request's number: copy the link, or open it on its host.
pub(crate) async fn pull_request_number(state: Store<UiModel>, url: String, provider: Option<String>, at: (f64, f64)) {
    let items = menu::pull_request_link(&t3_client::pull_request::open_on_host_label(provider.as_deref()));
    match crate::context_menu::show("Pull request link", items, Some(at)).await.as_deref() {
        Some("copy-link") => copy_text(&url),
        Some("open-external") => match web_link(&url) {
            Some(url) => open_external(&url),
            None => notice::push(state, Level::Error, "Could not open the link", url),
        },
        _ => {}
    }
}

/// What the server lets a file's menu do: reveal in the file manager, and the editors it found.
pub(crate) fn file_capabilities(state: Store<UiModel>) -> menu::FileCapabilities {
    let field = state.typed_config();
    let config = field.read();
    let Some(config) = config.as_ref() else { return menu::FileCapabilities::default() };
    let editors = crate::open_in::available(state);
    let file_manager = editors.iter().any(|id| id == t3_client::editors::FILE_MANAGER);
    let os = serde_json::to_value(&config.environment.platform.os).ok().and_then(|os| os.as_str().map(str::to_owned));
    let kind = config.shell_reveal_in_file_manager_kind.as_ref().and_then(|kind| serde_json::to_value(kind).ok()).and_then(|kind| kind.as_str().map(str::to_owned));
    menu::FileCapabilities {
        reveal_label: (config.shell_reveal_in_file_manager == Some(true) && file_manager).then(|| menu::reveal_label(os.as_deref(), kind.as_deref()).to_owned()),
        can_open_default: file_manager,
        editor_ids: editors,
    }
}

/// Asks the server to open `path` (an absolute path) in an editor (`shell.openInEditor`), or to reveal it.
pub(crate) fn open_path(state: Store<UiModel>, transport: &TransportHandle, path: String, editor: String, reveal: bool) {
    if !crate::settings_common::connected_allows(state, t3_contracts::AuthEnvironmentScope::OrchestrationOperate) {
        return;
    }
    let transport = transport.clone();
    spawn(async move {
        let mut payload = serde_json::json!({"cwd":path,"editor":editor});
        if reveal {
            payload["reveal"] = serde_json::json!(true);
        }
        let owner = runtime::response_owner(&transport, state);
        let answer = runtime::request_value_reported(transport.clone(), state, "shell.openInEditor", payload).await;
        if owner.is_some() && owner == runtime::response_owner(&transport, state) {
            if let Err(message) = answer {
                notice::push(state, Level::Error, if reveal { "Unable to reveal file" } else { "Could not open file" }, message);
            }
        }
    });
}

/// The menu of a workspace file at the pointer (`useFileContextMenu().show`): open with the default app, reveal, open with an editor.
/// `extra` are the items the surface adds after them, and `then` carries out an answer that is one of them.
pub(crate) async fn file(state: Store<UiModel>, transport: TransportHandle, absolute_path: Option<String>, at: Option<(f64, f64)>, extra: Vec<menu::Item>) -> Option<String> {
    let capabilities = file_capabilities(state);
    let mut items = menu::file(absolute_path.is_some(), &capabilities);
    items.extend(extra);
    let choice = crate::context_menu::show("File actions", items, at).await?;
    let (Some(path), true) = (absolute_path, choice == "open" || choice == "reveal-in-folder" || choice.starts_with("editor:")) else { return Some(choice) };
    let reveal = choice == "reveal-in-folder";
    let editor = if choice == "open" || reveal { t3_client::editors::FILE_MANAGER.to_owned() } else { choice["editor:".len()..].to_owned() };
    if choice != "open" && !reveal && !capabilities.editor_ids.contains(&editor) {
        return None;
    }
    open_path(state, &transport, path, editor, reveal);
    None
}

/// The right-click handler of changed-file rows (`useFileContextMenuHandler`): the file's menu at the pointer. None where the thread's
/// folder or the connection to ask is not known, which is where the surface has no host to ask through.
pub(crate) fn file_context() -> Option<Callback<(String, (f64, f64))>> {
    let (state, transport) = match crate::workspace_files::host() {
        Some(host) => (host.state, host.transport),
        None => {
            let host = try_consume_context::<crate::diff_panel::ExpandHost>()?;
            (host.state, host.transport)
        }
    };
    Some(Callback::new(move |(path, at): (String, (f64, f64))| {
        let transport = transport.clone();
        spawn(async move {
            let absolute = crate::open_in::thread_cwd(state).map(|root| t3_client::file_links::editor_path(&t3_client::file_links::FileTarget { path, line: None, column: None }, &root));
            file(state, transport, absolute, Some(at), vec![]).await;
        });
    }))
}

/// The menu of a picture or video in a message at the pointer (`MediaActions.tsx`): copy where it came from, open it in the file
/// viewer, save it, copy the picture. `url` is the address it is loaded from; `open_file` shows it in the file viewer.
pub(crate) async fn media(state: Store<UiModel>, noun: &'static str, name: String, url: Option<String>, reference: Option<menu::MediaReference>, open_file: Option<Callback<()>>, at: (f64, f64)) {
    let items = menu::media(noun, reference.as_ref(), open_file.is_some(), url.is_none(), true);
    let Some(choice) = crate::context_menu::show("Media actions", items, Some(at)).await else { return };
    let text = match (choice.as_str(), &reference) {
        ("copy-full-path", Some(menu::MediaReference::File { path, .. })) => Some(path.clone()),
        ("copy-relative-path", Some(menu::MediaReference::File { relative_path, .. })) => relative_path.clone(),
        ("copy-url", Some(menu::MediaReference::Url(url))) => Some(url.clone()),
        _ => None,
    };
    if let Some(text) = text {
        copy_text(&text);
        return;
    }
    match (choice.as_str(), url) {
        ("open-file", _) => {
            if let Some(open_file) = open_file {
                open_file.call(());
            }
        }
        ("save", Some(url)) => save_media(state, url, name),
        ("copy-image", Some(url)) => {
            let url = serde_json::to_string(&url).unwrap_or_default();
            let _ = document::eval(&format!("(async()=>{{try{{const blob=await (await fetch({url})).blob();await navigator.clipboard.write([new ClipboardItem({{[blob.type||'image/png']:blob}})]);}}catch(error){{console.warn('Copy image:',error);}}}})()"));
        }
        _ => {}
    }
}

/// Saves what `url` serves under `name`: the page downloads it; the desktop app asks where to put it.
fn save_media(state: Store<UiModel>, url: String, name: String) {
    #[cfg(feature = "desktop")]
    {
        spawn(async move {
            let Some(path) = crate::desktop_shell::pick_save_path(&name).await else { return };
            let answer = async { reqwest::get(&url).await?.error_for_status()?.bytes().await }.await;
            match answer.map_err(|error| error.to_string()).and_then(|bytes| std::fs::write(&path, &bytes).map_err(|error| error.to_string())) {
                Ok(()) => notice::push(state, Level::Success, "Download started", path),
                Err(message) => notice::push(state, Level::Error, "Could not save the file", message),
            }
        });
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = state;
        let (url, name) = (serde_json::to_string(&url).unwrap_or_default(), serde_json::to_string(&name).unwrap_or_default());
        let _ = document::eval(&format!("(async()=>{{try{{const blob=await (await fetch({url})).blob();const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download={name};document.body.appendChild(a);a.click();a.remove();setTimeout(()=>URL.revokeObjectURL(a.href),1000);}}catch(error){{console.warn('Save:',error);}}}})()"));
    }
}
