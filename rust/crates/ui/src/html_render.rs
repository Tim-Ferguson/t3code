//! An agent's HTML render, the page the `html_render` tool published, shown in a sandboxed frame: inline in the timeline
//! (`HtmlRenderTimelineRow`, `HtmlRenderFrame.tsx`) and as a tab of the right panel (`AttachmentFilePreview.tsx`). The frame's
//! behaviour is `BrowserDocumentFrame.tsx`'s `HtmlRenderDocument`; what can be decided without a page is `t3_client::html_render`.
//!
//! The page runs with `sandbox="allow-scripts allow-forms"` and an opaque origin, so it cannot reach the app's session or
//! storage; it wears the app theme (handed over in the URL fragment before first paint, then posted when the theme changes) and
//! asks the app, by message, to open a link or to fit its frame. The frame keeps its first URL for its lifetime: the signed URL
//! would be minted anew while the frame stays mounted, and a new `src` would reload the page.
use crate::{
    attachments::asset_url,
    link_menus::open_external,
    runtime::{self, UiModel, UiModelStoreExt},
    themes::Themes,
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{connection::ConnectionStatus, html_render as logic};

/// The theme and fonts the app wears now, as a render is handed them (`useHtmlRenderTheme`). Read in a component it follows the
/// theme; where no theme service is mounted it is the stock look.
fn current_theme(state: Store<UiModel>) -> logic::Theme {
    let settings = state.client_settings();
    let (sans, mono) = {
        let settings = settings.read();
        (settings.font_family_sans.0.clone(), settings.font_family_code.0.clone())
    };
    match try_consume_context::<Themes>() {
        Some(themes) => themes.html_render_theme(&sans, &mono),
        None => {
            use t3_client::themes::{Appearance, Catalog};
            let catalog = Catalog::default();
            logic::theme(
                &catalog.data.standard[&Appearance::Light],
                Appearance::Light,
                &logic::font_stack(&sans, logic::DEFAULT_SANS_FONT_STACK),
                &logic::font_stack(&mono, logic::DEFAULT_CODE_FONT_STACK),
            )
        }
    }
}

/// The signed URL of a published page (an inline attachment of the thread), asked for on the connection of the environment that
/// owns it (`assets.createUrl`).
async fn page_url(transport: &runtime::TransportHandle, state: Store<UiModel>, environment_key: &str, attachment_id: &str, file_name: &str) -> Result<String, String> {
    let resource = json!({"_tag":"attachment","attachmentId":attachment_id,"fileName":file_name,"mimeType":"text/html","disposition":"inline"});
    asset_url(transport, state, environment_key, resource).await.map(|preview| preview.url)
}

/// A sandboxed agent HTML render in the app theme (`HtmlRenderDocument`). `on_width` hears the width of the frame's box (its
/// parent), whenever it changes; `on_height` the page's own content height, so an inline frame can fit it.
#[component]
pub(crate) fn Document(state: Store<UiModel>, src: String, title: String, class: String, on_width: EventHandler<f64>, on_height: EventHandler<f64>) -> Element {
    let id = use_hook(|| format!("html-render-{}", uuid::Uuid::new_v4()));
    let theme = current_theme(state);
    let first = use_hook({
        let (src, theme) = (src.clone(), theme.clone());
        move || logic::framed_url(&src, &theme)
    });
    let mut loaded = use_signal(|| false);
    let mut page = use_signal(|| None::<document::Eval>);
    // The theme now in force, for the frame's task to post when the page has loaded.
    let latest = use_hook(|| std::rc::Rc::new(std::cell::RefCell::new(theme.clone())));
    *latest.borrow_mut() = theme.clone();
    use_future({
        let (id, latest) = (id.clone(), latest.clone());
        move || {
            let (id, latest) = (id.clone(), latest.clone());
            async move {
                let mut eval = document::eval(&format!("const args={};\n{}", json!({"id":id}), include_str!("../assets/html_render_abi.js")));
                page.set(Some(eval));
                while let Ok(message) = eval.recv::<Value>().await {
                    match message["type"].as_str() {
                        Some("width") => {
                            if let Some(width) = message["width"].as_f64() {
                                on_width.call(width);
                            }
                        }
                        Some("load") => {
                            loaded.set(true);
                            // Covers a theme change that landed while the page was loading.
                            let _ = eval.send(json!({"type":"post","message":logic::theme_message(&latest.borrow())}));
                        }
                        Some("message") => {
                            let data = &message["data"];
                            if let Some(height) = logic::read_content_height(data) {
                                on_height.call(height);
                            } else if let Some(request) = logic::read_link_request(data) {
                                // The page cannot open windows itself; it asks, and the app opens the link only while the frame
                                // has focus and the reader has just used the app.
                                if logic::may_open_link(message["focused"] == true, message["active"].as_bool()) {
                                    open_external(&request.url);
                                    let _ = eval.send(json!({"type":"post","message":logic::result_message(&request.id)}));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    });
    use_effect(use_reactive((&theme,), move |(theme,)| {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(json!({"type":"post","message":logic::theme_message(&theme)}));
        }
    }));
    use_drop(move || {
        if let Some(eval) = *page.peek() {
            let _ = eval.send(json!({"type":"stop"}));
        }
    });
    // A frame whose color scheme differs from its document's paints an opaque canvas, so the blank document a frame starts with
    // would flash white in dark mode. Once the page is in, its prefers-color-scheme follows the app.
    let scheme = if loaded() { Some(format!("color-scheme: {}", theme.appearance.key())) } else { None };
    rsx! {
        iframe {
            id: "{id}",
            class: "html-render-document {class}",
            src: "{first}",
            title: "{title}",
            "aria-label": "{title}",
            // Never allow-same-origin: the opaque origin keeps the page out of the app's session.
            "sandbox": logic::SANDBOX,
            "loading": "lazy",
            style: scheme,
        }
    }
}

/// An agent's HTML render inline in the thread: the page on the thread's own background, at the server's measured height for this
/// width until the page reports its own. Loading and failure hold the same box so nothing below it moves
/// (`HtmlRenderFrame.tsx`). `on_open` opens the page in the right panel.
#[component]
pub(crate) fn Frame(state: Store<UiModel>, transport: runtime::TransportHandle, environment_key: String, page: logic::Reference, on_open: EventHandler<()>) -> Element {
    let mut width = use_signal(|| logic::COLUMN_WIDTH);
    // Client fonts can wrap a page taller than the server measured it; a frame left short would scroll inside the thread and
    // take the reader's scroll.
    let mut content_height = use_signal(|| None::<f64>);
    let mut src = use_signal(|| None::<String>);
    let mut failed = use_signal(|| false);
    let mut asking = use_signal(|| false);
    {
        let (transport, environment_key, page) = (transport.clone(), environment_key.clone(), page.clone());
        // The URL needs a connection; it is asked for again when one returns. At most one mint per mount succeeds: the page keeps
        // its first URL.
        use_effect(move || {
            if *state.status().read() != ConnectionStatus::Connected || src.peek().is_some() || *asking.peek() {
                return;
            }
            asking.set(true);
            failed.set(false);
            let (transport, environment_key, page) = (transport.clone(), environment_key.clone(), page.clone());
            spawn(async move {
                match page_url(&transport, state, &environment_key, &page.attachment_id, &logic::file_name(&page.title)).await {
                    Ok(url) => src.set(Some(url)),
                    Err(_) => failed.set(true),
                }
                asking.set(false);
            });
        });
    }
    let height = logic::frame_height(&page, width(), content_height());
    let title = page.title.clone();
    rsx! {
        div { class: "html-render-frame", style: "height: {height}px", "data-html-render": "{page.attachment_id}",
            if let Some(url) = src() {
                Document { state, src: url, title: title.clone(), class: "html-render-inline".to_owned(),
                    on_width: move |next: f64| { if (next - *width.peek()).abs() >= 0.5 { width.set(next); } },
                    on_height: move |next: f64| { if *content_height.peek() != Some(next) { content_height.set(Some(next)); } } }
                div { class: "html-render-open",
                    button { class: "icon-button", r#type: "button", "aria-label": "Open in panel", title: "Open in panel", onclick: move |_| on_open.call(()), "\u{2922}" }
                }
            } else if failed() {
                p { class: "html-render-failed muted", "Unable to load {title}" }
            }
        }
    }
}

/// The page as a tab of the right panel (`AttachmentFilePreview` with `htmlRender`): the render in the app theme, and the page
/// as a download.
#[component]
pub(crate) fn PanelTab(state: Store<UiModel>, transport: runtime::TransportHandle, environment_key: String, attachment_id: String, name: String) -> Element {
    let mut src = use_signal(|| None::<String>);
    let mut download = use_signal(|| None::<String>);
    let mut failed = use_signal(|| false);
    let mut asking = use_signal(|| false);
    {
        let (transport, environment_key, attachment_id, name) = (transport.clone(), environment_key.clone(), attachment_id.clone(), name.clone());
        use_effect(move || {
            if *state.status().read() != ConnectionStatus::Connected || src.peek().is_some() || *asking.peek() {
                return;
            }
            asking.set(true);
            failed.set(false);
            let (transport, environment_key, attachment_id, name) = (transport.clone(), environment_key.clone(), attachment_id.clone(), name.clone());
            spawn(async move {
                let inline = page_url(&transport, state, &environment_key, &attachment_id, &name).await;
                let attachment = json!({"_tag":"attachment","attachmentId":attachment_id,"fileName":name,"mimeType":"text/html","disposition":"attachment"});
                let saved = asset_url(&transport, state, &environment_key, attachment).await;
                match inline {
                    Ok(url) => {
                        src.set(Some(url));
                        download.set(saved.ok().map(|preview| preview.url));
                    }
                    Err(_) => failed.set(true),
                }
                asking.set(false);
            });
        });
    }
    rsx! {
        section { class: "html-render-panel", "aria-label": "Page {name}",
            header { class: "html-render-panel-header",
                strong { "{name}" }
                if let Some(url) = download() {
                    a { class: "link-button", href: "{url}", download: "{name}", "aria-label": "Download {name}", "Download" }
                }
            }
            if let Some(url) = src() {
                Document { state, src: url, title: name.trim_end_matches(".html").to_owned(), class: "html-render-panel-frame".to_owned(), on_width: |_| {}, on_height: |_| {} }
            } else if failed() {
                p { class: "html-render-failed muted", role: "alert", "Unable to load {name}" }
            } else {
                p { class: "muted", "Loading {name}\u{2026}" }
            }
        }
    }
}
