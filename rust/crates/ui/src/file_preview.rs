//! Workspace files shown in the page: the image a tool looked at, and a project's favicon. Both
//! load through a signed asset URL the server issues for the exact resource (`assets.createUrl`),
//! resolved against the connected environment, never from a path built here. An answer for another
//! thread, project or connection than the one on screen is dropped, and a file the server will not
//! serve leaves a plain line (or nothing), not a broken image.
use crate::{
    attachments::asset_url,
    runtime::{self, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};
use t3_client::{attachments::Preview, connection::ConnectionStatus};

/// The image a tool call viewed, loaded only while its row is open.
#[component]
pub(crate) fn WorkspaceImage(state: Store<UiModel>, transport: runtime::TransportHandle, environment_key: String, thread_id: String, path: String) -> Element {
    let resource = use_resource({
        let (transport, environment_key, thread_id, path) = (transport.clone(), environment_key.clone(), thread_id.clone(), path.clone());
        move || {
            let (transport, environment_key, thread_id, path) = (transport.clone(), environment_key.clone(), thread_id.clone(), path.clone());
            // Loaded again when the connection comes back; a signed URL is only asked for while connected.
            let connected = *state.status().read() == ConnectionStatus::Connected;
            async move {
                if !connected {
                    return Some(Err("Not connected".to_owned()));
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = asset_url(&transport, state, &environment_key, json!({"_tag":"media-file","threadId":thread_id,"path":path})).await;
                // Another connection answered by now: not this row's picture.
                (owner.is_some() && owner == runtime::response_owner(&transport, state)).then_some(answer)
            }
        }
    });
    let name = path.rsplit(['/', '\\']).next().unwrap_or(&path).to_owned();
    let shown = resource.read();
    match &*shown {
        None => rsx! {p {class:"muted",role:"status","Loading image\u{2026}"}},
        Some(None) => rsx! {},
        Some(Some(Err(reason))) if reason == "Not connected" => rsx! {p {class:"muted",role:"status","Not connected, so the image cannot be loaded."}},
        Some(Some(Err(_))) => rsx! {p {class:"muted","The image is no longer available."}},
        Some(Some(Ok(Preview { url, width, height }))) => rsx! {
            img {class:"viewed-image",role:"img","aria-label":"Viewed image {name}",alt:"{name}",src:"{url}",loading:"lazy",width:width.map(|width| width.to_string()),height:height.map(|height| height.to_string())}
        },
    }
}

/// A project's own icon beside its name when it has one the server will serve, else a plain mark.
#[component]
pub(crate) fn ProjectFavicon(state: Store<UiModel>, transport: runtime::TransportHandle, environment_key: String, cwd: String) -> Element {
    let resource = use_resource({
        let (transport, environment_key, cwd) = (transport.clone(), environment_key.clone(), cwd.clone());
        move || {
            let (transport, environment_key, cwd) = (transport.clone(), environment_key.clone(), cwd.clone());
            let connected = *state.status().read() == ConnectionStatus::Connected;
            async move {
                if !connected {
                    return None;
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = asset_url(&transport, state, &environment_key, json!({"_tag":"project-favicon","cwd":cwd})).await.ok();
                if owner.is_some() && owner == runtime::response_owner(&transport, state) { answer } else { None }
            }
        }
    });
    let shown = resource.read();
    match &*shown {
        Some(Some(preview)) => rsx! {img {class:"project-favicon","aria-hidden":"true",alt:"",src:"{preview.url}",width:"14",height:"14",loading:"lazy"}},
        _ => rsx! {span {class:"project-icon","aria-hidden":"true","\u{25c7}"}},
    }
}

/// The path a row's item viewed, if it names an image.
pub(crate) fn viewed_path(item: &Value) -> Option<String> {
    t3_client::timeline_rows::viewed_image_path(item)
}
