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

fn cwd(host: &Host) -> Option<String> {
    open_in::thread_cwd(host.state)
}

/// A link that names a file. Pictures and PDFs inside the workspace open in the page; the rest
/// go to the editor, at the line the link names.
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
            let editor = open_in::favorite(host.state, last.as_ref().and_then(|last| last.0.peek().clone()).as_deref());
            if let (Some(editor), Some(last)) = (editor, last) {
                open_in::open(&host.transport, host.state, busy, last.0, logic::editor_path(&target, &root), editor, false);
            } else {
                crate::notice::push(host.state, crate::notice::Level::Info, "No editor to open the file in", target.path.clone());
            }
        }
    };
    rsx! { a { class: "file-link", href: "#", role: "button", title: "{title}", "aria-label": "{label}", onclick, {children} } }
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
                let answer = asset_url(&host.transport, host.state, &host.environment_key, json!({"_tag":"workspace-file","threadId":host.thread_id,"path":relative})).await;
                // Another connection or thread answered by now: not this picture.
                (owner.is_some() && owner == runtime::response_owner(&host.transport, host.state)).then_some(answer)
            }
        }
    });
    if relative.is_none() {
        return rsx! { span { class: "muted", "{alt}" } };
    }
    let shown = resource.read();
    match &*shown {
        None => rsx! { span { class: "muted", role: "status", "Loading image\u{2026}" } },
        Some(None) => rsx! {},
        Some(Some(Err(_))) => rsx! { span { class: "muted", "{alt} (image unavailable)" } },
        Some(Some(Ok(Signed { url, .. }))) => rsx! { img { class: "markdown-workspace-image", src: "{url}", alt: "{alt}", title: "{title}", "aria-label": "Image {alt}", loading: "lazy" } },
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
                let answer = asset_url(&host.transport, host.state, &host.environment_key, json!({"_tag":"workspace-file","threadId":host.thread_id,"path":path})).await;
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
                (Some(Some(Ok(signed))), _) => rsx! { img { class: "file-preview-image", src: "{signed.url}", alt: "{name}", "aria-label": "Preview image {name}" } },
            }
            div { class: "dialog-actions", button { r#type: "button", "aria-label": "Close preview", onclick: move |_| preview.set(None), "Close" } }
        }
    } }
}
