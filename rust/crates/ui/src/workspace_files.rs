//! Files of the workspace named in a message: a link opens an image or PDF in the page, or any other
//! file in the editor at its line, and an image in the Markdown shows through the server. Whatever
//! is shown is loaded through a signed asset URL the server issues for that exact path
//! (`assets.createUrl`); a path is never turned into a URL here, and file content is never put in the
//! page as markup. Port of the file links and media of `MarkdownCodeLink`/`FilePreview`.
use crate::{
    attachments::asset_url,
    open_in,
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{connection::ConnectionStatus, file_links as logic, attachments::Preview as Signed};

/// What the Markdown in a thread's timeline needs to resolve a path: the thread it belongs to.
#[derive(Clone, PartialEq)]
pub(crate) struct Host {
    pub state: Store<UiModel>,
    pub transport: TransportHandle,
    pub environment_key: String,
    pub thread_id: String,
    /// The file being previewed, as the workspace-relative path.
    pub preview: Signal<Option<String>>,
}

/// Whether the Markdown being drawn belongs to a thread whose files can be resolved.
pub(crate) fn host_available() -> bool {
    try_consume_context::<Host>().is_some()
}

/// The host of the Markdown being drawn, if it belongs to a thread.
pub(crate) fn host() -> Option<Host> {
    try_consume_context::<Host>()
}

fn cwd(host: &Host) -> Option<String> {
    open_in::thread_cwd(host.state)
}

/// Opens the file in the editor, at the line the link names, in the favorite one.
fn open_in_editor(host: &Host, target: &logic::FileTarget, busy: Signal<bool>, last: Option<open_in::Last>) {
    let Some(root) = cwd(host) else { return };
    let editor = open_in::favorite(host.state, last.as_ref().and_then(|last| last.0.peek().clone()).as_deref());
    if let (Some(editor), Some(last)) = (editor, last) {
        open_in::open(&host.transport, host.state, busy, last.0, logic::editor_path(target, &root), editor, false);
    } else {
        crate::notice::push(host.state, crate::notice::Level::Info, "No editor to open the file in", target.path.clone());
    }
}

/// A link that names a file. Pictures and PDFs inside the workspace open in the page; the rest
/// go to the editor, at the line the link names. A right-click offers the same and more (`ChatMarkdown.tsx`'s file chip).
#[component]
pub(crate) fn FileLink(target: logic::FileTarget, title: String, children: Element) -> Element {
    let host = use_context::<Host>();
    let busy = use_signal(|| false);
    let last = try_consume_context::<open_in::Last>();
    let label = format!("Open {}{}", target.path, target.line.map(|line| format!(" at line {line}")).unwrap_or_default());
    let onclick = {
        let (host, target) = (host.clone(), target.clone());
        move |event: MouseEvent| {
            event.prevent_default();
            let Some(root) = cwd(&host) else { return };
            if logic::previewable(&target.path).is_some() {
                if let Some(relative) = logic::workspace_relative(&target.path, &root) {
                    let mut preview = host.preview;
                    preview.set(Some(relative));
                    return;
                }
            }
            open_in_editor(&host, &target, busy, last);
        }
    };
    let oncontextmenu = {
        let (host, target) = (host.clone(), target.clone());
        move |event: MouseEvent| {
            event.prevent_default();
            event.stop_propagation();
            let at = event.client_coordinates();
            let (host, target) = (host.clone(), target.clone());
            spawn(async move { file_menu(host, target, busy, last, (at.x, at.y)).await });
        }
    };
    rsx! { a { class: "file-link", href: "#", role: "button", title: "{title}", "aria-label": "{label}", onclick, oncontextmenu, {children} } }
}

/// The menu of a file named in a message: preview it when it is media, open it in the editor, reveal it, copy its path.
async fn file_menu(host: Host, target: logic::FileTarget, busy: Signal<bool>, last: Option<open_in::Last>, at: (f64, f64)) {
    use t3_client::context_menu::Item;
    let Some(root) = cwd(&host) else { return };
    let relative = logic::workspace_relative(&target.path, &root);
    let absolute = logic::editor_path(&logic::FileTarget { path: target.path.clone(), line: None, column: None }, &root);
    let editor = open_in::favorite(host.state, last.as_ref().and_then(|last| last.0.peek().clone()).as_deref());
    let capabilities = crate::link_menus::file_capabilities(host.state);
    let mut items = vec![];
    if logic::previewable(&target.path).is_some() && relative.is_some() {
        items.push(Item::new("preview-media", "Preview media"));
    }
    if editor.is_some() {
        items.push(Item::new("open", t3_client::editors::menu_label(editor.as_deref())));
    }
    if let Some(reveal) = capabilities.reveal_label {
        items.push(Item::new("reveal", reveal));
    }
    items.push(Item::new("copy-relative", "Copy relative path"));
    items.push(Item::new("copy-full", "Copy full path"));
    let Some(choice) = crate::context_menu::show("File actions", items, Some(at)).await else { return };
    match choice.as_str() {
        "preview-media" => {
            let mut preview = host.preview;
            preview.set(relative);
        }
        "open" => open_in_editor(&host, &target, busy, last),
        "reveal" => crate::link_menus::open_path(host.state, &host.transport, absolute, t3_client::editors::FILE_MANAGER.to_owned(), true),
        "copy-relative" => crate::sidebar_rows::copy_text(relative.as_deref().unwrap_or(&target.path)),
        "copy-full" => crate::sidebar_rows::copy_text(&absolute),
        _ => {}
    }
}

/// An image in the Markdown that points into the workspace, loaded through the server.
#[component]
pub(crate) fn MarkdownImage(src: String, alt: String, title: String) -> Element {
    let host = use_context::<Host>();
    let target = logic::target(&src);
    let relative = target.as_ref().and_then(|target| cwd(&host).and_then(|root| logic::workspace_relative(&target.path, &root)));
    let resource = use_resource({
        let host = host.clone();
        let relative = relative.clone();
        move || {
            let (host, relative) = (host.clone(), relative.clone());
            let connected = *host.state.status().read() == ConnectionStatus::Connected;
            async move {
                let relative = relative?;
                if !connected {
                    return Some(Err("Not connected".to_owned()));
                }
                let owner = runtime::response_owner(&host.transport, host.state);
                let tag = if matches!(logic::previewable(&relative), Some(logic::Preview::Video)) { "media-file" } else { "workspace-file" };
                let answer = asset_url(&host.transport, host.state, &host.environment_key, json!({"_tag":tag,"threadId":host.thread_id,"path":relative})).await;
                // Another connection or thread answered by now: not this picture.
                (owner.is_some() && owner == runtime::response_owner(&host.transport, host.state)).then_some(answer)
            }
        }
    });
    if relative.is_none() {
        return rsx! { span { class: "muted", "{alt}" } };
    }
    let is_video = relative.as_deref().is_some_and(|path| logic::previewable(path) == Some(logic::Preview::Video));
    let shown = resource.read();
    match &*shown {
        None => rsx! { span { class: "muted", role: "status", "Loading image\u{2026}" } },
        Some(None) => rsx! {},
        Some(Some(Err(_))) if is_video => rsx! { span { class: "muted", "{alt} (video unavailable)" } },
        Some(Some(Err(_))) => rsx! { span { class: "muted", "{alt} (image unavailable)" } },
        Some(Some(Ok(Signed { url, .. }))) if is_video => {
            let (menu_host, menu_url, name, relative, absolute) = (host.clone(), url.clone(), alt.clone(), relative.clone(), target.as_ref().and_then(|target| cwd(&host).map(|root| logic::editor_path(&logic::FileTarget { path: target.path.clone(), line: None, column: None }, &root))));
            rsx! { video { class: "markdown-workspace-video", src: "{url}", controls: true, preload: "metadata", title: "{title}", "aria-label": "Video {alt}",
                oncontextmenu: move |event| {
                    event.prevent_default();
                    event.stop_propagation();
                    let at = event.client_coordinates();
                    let (host, url, name) = (menu_host.clone(), menu_url.clone(), name.clone());
                    let reference = absolute.clone().map(|path| t3_client::context_menu::MediaReference::File { path, relative_path: relative.clone() });
                    let open_file = relative.clone().map(|relative| { let mut preview = host.preview; Callback::new(move |()| preview.set(Some(relative.clone()))) });
                    spawn(async move { crate::link_menus::media(host.state, "video", name, Some(url), reference, open_file, (at.x, at.y)).await });
                } } }
        }
        Some(Some(Ok(Signed { url, .. }))) => {
            let (menu_host, menu_url, name, relative, absolute) = (host.clone(), url.clone(), alt.clone(), relative.clone(), target.as_ref().and_then(|target| cwd(&host).map(|root| logic::editor_path(&logic::FileTarget { path: target.path.clone(), line: None, column: None }, &root))));
            rsx! { img { class: "markdown-workspace-image", src: "{url}", alt: "{alt}", title: "{title}", "aria-label": "Image {alt}", loading: "lazy",
                oncontextmenu: move |event| {
                    event.prevent_default();
                    event.stop_propagation();
                    let at = event.client_coordinates();
                    let (host, url, name) = (menu_host.clone(), menu_url.clone(), name.clone());
                    let reference = absolute.clone().map(|path| t3_client::context_menu::MediaReference::File { path, relative_path: relative.clone() });
                    let open_file = relative.clone().map(|relative| { let mut preview = host.preview; Callback::new(move |()| preview.set(Some(relative.clone()))) });
                    spawn(async move { crate::link_menus::media(host.state, "image", name, Some(url), reference, open_file, (at.x, at.y)).await });
                } } }
        }
    }
}

/// The preview of a workspace file: an image, or a PDF in a frame, both from the signed URL.
#[component]
pub(crate) fn PreviewDialog() -> Element {
    let Some(host) = try_consume_context::<Host>() else { return rsx! {} };
    let Some(path) = host.preview.read().clone() else { return rsx! {} };
    rsx! { Open { key: "{path}", host, path } }
}

#[component]
fn Open(host: Host, path: String) -> Element {
    let kind = logic::previewable(&path);
    let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
    let resource = use_resource({
        let (host, path) = (host.clone(), path.clone());
        move || {
            let (host, path) = (host.clone(), path.clone());
            let connected = *host.state.status().read() == ConnectionStatus::Connected;
            async move {
                if !connected {
                    return Some(Err("Not connected".to_owned()));
                }
                let owner = runtime::response_owner(&host.transport, host.state);
                // Played media is served with byte ranges from the media route; a picture or document as a workspace file.
                let tag = if matches!(logic::previewable(&path), Some(logic::Preview::Video | logic::Preview::Audio)) { "media-file" } else { "workspace-file" };
                let answer = asset_url(&host.transport, host.state, &host.environment_key, json!({"_tag":tag,"threadId":host.thread_id,"path":path})).await;
                (owner.is_some() && owner == runtime::response_owner(&host.transport, host.state)).then_some(answer)
            }
        }
    });
    let mut preview = host.preview;
    let shown = resource.read();
    rsx! { div { class: "modal-backdrop", onclick: move |_| preview.set(None),
        div { class: "dialog file-preview-dialog", role: "dialog", "aria-modal": "true", "aria-label": "Preview {name}", tabindex: "-1", onclick: move |event| event.stop_propagation(),
            onmounted: move |event| { let element = event.data(); spawn(async move { let _ = element.set_focus(true).await; }); },
            onkeydown: move |event| { if event.key() == Key::Escape { event.stop_propagation(); preview.set(None); } },
            h2 { "{name}" }
            match (&*shown, kind) {
                (None, _) => rsx! { p { class: "muted", role: "status", "Loading\u{2026}" } },
                (Some(None), _) => rsx! {},
                (Some(Some(Err(reason))), _) => rsx! { p { class: "error-banner", role: "alert", "This file cannot be previewed. {reason}" } },
                (Some(Some(Ok(signed))), Some(logic::Preview::Pdf)) => rsx! { iframe { class: "file-preview-frame", title: "{name}", "aria-label": "Preview frame {name}", src: "{signed.url}", "sandbox": "" } },
                (Some(Some(Ok(signed))), Some(logic::Preview::Video)) => rsx! { video { class: "file-preview-video", controls: true, preload: "metadata", src: "{signed.url}", "aria-label": "Preview video {name}" } },
                (Some(Some(Ok(signed))), Some(logic::Preview::Audio)) => rsx! { audio { class: "file-preview-audio", controls: true, preload: "metadata", src: "{signed.url}", "aria-label": "Preview audio {name}" } },
                (Some(Some(Ok(signed))), _) => rsx! { img { class: "file-preview-image", src: "{signed.url}", alt: "{name}", "aria-label": "Preview image {name}" } },
            }
            div { class: "dialog-actions", button { r#type: "button", "aria-label": "Close preview", onclick: move |_| preview.set(None), "Close" } }
        }
    } }
}
