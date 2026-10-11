//! A workspace file the file panel plays or frames instead of reading as text: audio, video and
//! PDF (`AudioPreview.tsx`, `WorkspaceVideoPreview`, `BrowserDocumentFrame.tsx`), and the table a CSV
//! or TSV file reads as (`DelimitedTablePreview.tsx`). Media loads through a signed asset URL the
//! server issues for the exact file (`assets.createUrl`), resolved against the connected environment.
use crate::{
    attachments::asset_url,
    runtime::{self, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::json;
use t3_client::{connection::ConnectionStatus, file_preview as logic};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Audio,
    Video,
    Pdf,
}
impl Kind {
    pub(crate) fn of(path: &str) -> Option<Self> {
        if logic::is_video_path(path) {
            Some(Self::Video)
        } else if logic::is_audio_path(path) {
            Some(Self::Audio)
        } else if logic::is_pdf_path(path) {
            Some(Self::Pdf)
        } else {
            None
        }
    }
    /// A PDF is a document of the workspace, audio and video are media files.
    fn resource(self) -> &'static str {
        if self == Self::Pdf { "workspace-file" } else { "media-file" }
    }
    fn noun(self) -> &'static str {
        match self {
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Pdf => "document",
        }
    }
}

#[component]
pub(crate) fn Media(state: Store<UiModel>, transport: runtime::TransportHandle, environment_key: String, thread_id: String, cwd: String, path: String, kind: Kind) -> Element {
    let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
    // Asked again when the connection comes back, and when a player gave up on the URL it had.
    let mut attempt = use_signal(|| 0u32);
    let resource = use_resource({
        let (transport, environment_key, thread_id, cwd, path) = (transport.clone(), environment_key.clone(), thread_id.clone(), cwd.clone(), path.clone());
        move || {
            let (transport, environment_key, thread_id, cwd, path) = (transport.clone(), environment_key.clone(), thread_id.clone(), cwd.clone(), path.clone());
            let connected = *state.status().read() == ConnectionStatus::Connected;
            let _ = attempt();
            async move {
                if !connected {
                    return Some(Err("Not connected".to_owned()));
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = asset_url(&transport, state, &environment_key, json!({"_tag":kind.resource(),"threadId":thread_id,"path":logic::absolute_path(&cwd, &path)})).await;
                // Another connection answered by now: not this file's.
                (owner.is_some() && owner == runtime::response_owner(&transport, state)).then_some(answer)
            }
        }
    });
    let mut failed = use_signal(|| None::<String>);
    let shown = resource.read();
    let noun = kind.noun();
    match &*shown {
        None => rsx! { p { class: "muted file-panel-note", role: "status", "Loading {noun}\u{2026}" } },
        Some(None) => rsx! {},
        Some(Some(Err(reason))) if reason == "Not connected" => rsx! { p { class: "muted file-panel-note", role: "status", "Not connected, so the {noun} cannot be loaded." } },
        Some(Some(Err(_))) => rsx! { div { class: "file-panel-note", role: "alert", p { class: "error-banner", "Unable to load {noun}." } button { r#type: "button", "aria-label": "Retry loading {noun}", onclick: move |_| { failed.set(None); attempt.set(attempt() + 1); }, "Retry" } } },
        Some(Some(Ok(preview))) if failed.read().as_deref() == Some(preview.url.as_str()) => rsx! { div { class: "file-panel-note", role: "alert", p { class: "error-banner", "Unable to load {noun}." } button { r#type: "button", "aria-label": "Retry loading {noun}", onclick: move |_| { failed.set(None); attempt.set(attempt() + 1); }, "Retry" } } },
        Some(Some(Ok(preview))) => {
            let url = preview.url.clone();
            let broken = url.clone();
            match kind {
                Kind::Audio => rsx! { div { class: "file-media", audio { controls: true, preload: "metadata", src: "{url}", "aria-label": "Audio {name}", onerror: move |_| failed.set(Some(broken.clone())) } } },
                Kind::Video => rsx! { div { class: "file-media", video { controls: true, preload: "metadata", src: "{url}", "aria-label": "Video {name}", onerror: move |_| failed.set(Some(broken.clone())) } } },
                // The built-in viewer needs an unsandboxed frame; a PDF runs no scripts.
                Kind::Pdf => rsx! { iframe { class: "file-document", src: "{url}{logic::PDF_VIEWER_FRAGMENT}", title: "{name}", "aria-label": "Document {name}" } },
            }
        }
    }
}

/// A bounded table for CSV and TSV text; the source view keeps every byte.
#[component]
pub(crate) fn Table(name: String, text: String, delimiter: char) -> Element {
    let table = use_memo(use_reactive((&text, &delimiter), |(text, delimiter)| logic::parse_table(&text, delimiter)));
    let table = table.read();
    let mut rows = table.rows.iter();
    let header = rows.next();
    rsx! {
        div { class: "file-table-panel",
            if table.truncated { p { class: "muted file-panel-note", role: "status", "{logic::TABLE_NOTICE}" } }
            div { class: "file-table-scroll",
                table { class: "file-table", "aria-label": "{name}",
                    if let Some(header) = header {
                        thead { tr { for (column, cell) in header.iter().enumerate() { th { key: "{column}", scope: "col", "{cell}" } } } }
                    }
                    tbody {
                        for (row, cells) in rows.enumerate() { tr { key: "{row}", for (column, cell) in cells.iter().enumerate() { td { key: "{column}", "{cell}" } } } }
                    }
                }
            }
        }
    }
}
