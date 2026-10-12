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

/// An image a tool returned inline, loaded only while its row is open. The bytes are not in the
/// timeline: the server numbers the images of an item and signs a URL for each (`tool-output-image`).
#[component]
pub(crate) fn ToolOutputImage(state: Store<UiModel>, transport: runtime::TransportHandle, environment_key: String, thread_id: String, item_id: String, index: usize) -> Element {
    let resource = use_resource({
        let (transport, environment_key, thread_id, item_id) = (transport.clone(), environment_key.clone(), thread_id.clone(), item_id.clone());
        move || {
            let (transport, environment_key, thread_id, item_id) = (transport.clone(), environment_key.clone(), thread_id.clone(), item_id.clone());
            let connected = *state.status().read() == ConnectionStatus::Connected;
            async move {
                if !connected {
                    return Some(Err("Not connected".to_owned()));
                }
                let owner = runtime::response_owner(&transport, state);
                let answer = asset_url(&transport, state, &environment_key, json!({"_tag":"tool-output-image","threadId":thread_id,"itemId":item_id,"index":index})).await;
                (owner.is_some() && owner == runtime::response_owner(&transport, state)).then_some(answer)
            }
        }
    });
    let shown = resource.read();
    match &*shown {
        None => rsx! {p {class:"muted",role:"status","Loading image\u{2026}"}},
        Some(None) => rsx! {},
        Some(Some(Err(reason))) if reason == "Not connected" => rsx! {p {class:"muted",role:"status","Not connected, so the image cannot be loaded."}},
        Some(Some(Err(_))) => rsx! {p {class:"muted","The image is no longer available."}},
        Some(Some(Ok(Preview { url, width, height }))) => rsx! {
            img {class:"viewed-image tool-output-image",role:"img","aria-label":"Tool output image {index + 1}",alt:"Tool output image",src:"{url}",loading:"lazy",width:width.map(|width| width.to_string()),height:height.map(|height| height.to_string())}
        },
    }
}

/// A project's own icon beside its name when it has one the server will serve, else a plain mark.
#[component]
pub(crate) fn ProjectFavicon(state: Store<UiModel>, transport: runtime::TransportHandle, environment_key: String, cwd: String, title: String) -> Element {
    let resource = use_resource({
        let (transport, environment_key, cwd) = (transport.clone(), environment_key.clone(), cwd.clone());
        move || {
            let (transport, environment_key, cwd) = (transport.clone(), environment_key.clone(), cwd.clone());
            // A project of another environment is asked of that environment, on its own connection.
            let connected = t3_contracts::EnvironmentId::new(&environment_key).map_or(*state.status().read() == ConnectionStatus::Connected, |environment| crate::fleet::status(state, &environment) == ConnectionStatus::Connected);
            async move {
                if !connected {
                    return None;
                }
                let owner = runtime::response_owner(&transport, state);
                // The window moving on from the connection leaves nothing to ask.
                owner.as_ref()?;
                let answer = asset_url(&transport, state, &environment_key, json!({"_tag":"project-favicon","cwd":cwd})).await.ok();
                if owner.is_some() && owner == runtime::response_owner(&transport, state) { answer } else { None }
            }
        }
    });
    let shown = resource.read();
    match &*shown {
        // The server's marker for "no icon" is never loaded: it would be a request that fails.
        Some(Some(preview)) if !t3_client::attachments::is_project_favicon_fallback(&preview.url) => rsx! {img {class:"project-favicon","aria-hidden":"true",alt:"",src:"{preview.url}",width:"14",height:"14",loading:"lazy"}},
        _ => {
            // No icon the server will serve: the project's first letter on a tile of its own colour.
            let letter = title.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "\u{25c7}".to_owned());
            let hue = title.bytes().fold(0u32, |hash, byte| hash.wrapping_mul(31).wrapping_add(u32::from(byte))) % 360;
            rsx! {span {class:"project-monogram","aria-hidden":"true",style:"color:oklch(0.5 0.14 {hue});background-color:color-mix(in srgb, currentColor 14%, transparent)","{letter}"}}
        }
    }
}

/// The path a row's item viewed, if it names an image.
pub(crate) fn viewed_path(item: &Value) -> Option<String> {
    t3_client::timeline_rows::viewed_image_path(item)
}
