//! Platform storage calls only; JSON decoding and preference behavior stay in Rust.
use crate::runtime::{UiModel, UiModelStoreExt};
use dioxus::prelude::*;
use t3_contracts::ClientSettings;

pub async fn hydrate(state: Store<UiModel>) {
    match read().await {
        Ok(settings) => {
            state.client_settings().set(settings);
            state.client_settings_error().set(None);
        }
        Err(error) => state.client_settings_error().set(Some(error)),
    }
}
async fn read() -> Result<ClientSettings, String> {
    #[cfg(target_arch = "wasm32")]
    let raw = {
        let storage = web_sys::window()
            .ok_or("Client storage is unavailable.")?
            .local_storage()
            .map_err(|_| "Could not read client storage. Saved preferences have been preserved.")?
            .ok_or("Client storage is unavailable.")?;
        storage.get_item("t3code:client-settings:v1").map_err(
            |_| "Could not read saved client preferences. Saved preferences have been preserved.",
        )?
    };
    #[cfg(not(target_arch = "wasm32"))]
    let raw =
        dioxus::document::eval("return window.localStorage.getItem('t3code:client-settings:v1');")
            .join::<Option<String>>()
            .await
            .map_err(|_| "Could not read client storage. Saved preferences have been preserved.")?;
    raw.map(|raw| {
        serde_json::from_str(&raw).map_err(|_| {
            "Saved client preferences are invalid. Saved preferences have been preserved."
                .to_string()
        })
    })
    .unwrap_or_else(|| Ok(ClientSettings::default()))
}
