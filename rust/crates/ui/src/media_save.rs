//! Keeps bytes the environment handed this device: a file a page downloaded, a screenshot of a device. The page downloads them; the
//! desktop app asks where to put them (`link_menus::save_media` does the same for a file that has an address of its own).
use crate::runtime::UiModel;
use dioxus::prelude::*;

#[cfg(test)]
thread_local! {
    /// What a test's window was asked to keep: the name and the bytes.
    pub(crate) static SAVED: std::cell::RefCell<Vec<(String, Vec<u8>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(all(feature = "desktop", not(test)))]
pub(crate) async fn save(state: Store<UiModel>, name: &str, bytes: Vec<u8>) {
    use crate::notice::{self, Level};
    let Some(path) = crate::desktop_shell::pick_save_path(name).await else { return };
    match std::fs::write(&path, &bytes) {
        Ok(()) => notice::push(state, Level::Success, "Saved", path),
        Err(error) => notice::push(state, Level::Error, "Could not save the file", error.to_string()),
    }
}

#[cfg(test)]
pub(crate) async fn save(_state: Store<UiModel>, name: &str, bytes: Vec<u8>) {
    SAVED.with(|saved| saved.borrow_mut().push((name.to_owned(), bytes)));
}

#[cfg(all(not(feature = "desktop"), not(test)))]
pub(crate) async fn save(_state: Store<UiModel>, name: &str, bytes: Vec<u8>) {
    use base64::Engine;
    let data = serde_json::to_string(&base64::engine::general_purpose::STANDARD.encode(&bytes)).unwrap_or_default();
    let name = serde_json::to_string(name).unwrap_or_default();
    let _ = document::eval(&format!("try{{const bytes=Uint8Array.from(atob({data}),c=>c.charCodeAt(0));const a=document.createElement('a');a.href=URL.createObjectURL(new Blob([bytes]));a.download={name};document.body.appendChild(a);a.click();a.remove();setTimeout(()=>URL.revokeObjectURL(a.href),1000);}}catch(error){{console.warn('Save:',error);}}"));
}
