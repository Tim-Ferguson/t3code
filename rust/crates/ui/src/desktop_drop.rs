//! Folders dropped on a composer. The original's window can read the path of what is dropped (`getPathForFile`), so a folder dropped
//! on the composer becomes a link to it in the prompt (`addDroppedFolders` of `ChatComposer.tsx`) and files upload as attachments.
//! The desktop window hands the path of each dropped item over; a page in a browser is given none, so there a folder is never told
//! from a file and nothing here changes what a drop does. A folder means something only to a server on this computer.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::{html::FileData, prelude::*};
use t3_client::draft_storage::{DraftKind, DraftTarget};

/// The folders among what was dropped (their paths) and the files left to upload.
pub(crate) fn split(dropped: Vec<FileData>) -> (Vec<String>, Vec<FileData>) {
    let mut folders = Vec::new();
    let mut files = Vec::new();
    for item in dropped {
        let path = item.path();
        if path.is_dir() {
            folders.push(path.to_string_lossy().into_owned());
        } else {
            files.push(item);
        }
    }
    (folders, files)
}

/// `serializeComposerFileLink`: `[name](path)` with the name's brackets escaped and the path encoded as a link's destination.
pub(crate) fn file_link(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let label = name.replace('\\', "\\\\").replace('[', "\\[").replace(']', "\\]");
    // `encodeURI` leaves these alone; the link then escapes the ones that would end it.
    let mut destination = String::new();
    for byte in path.bytes() {
        match byte {
            b'(' => destination.push_str("%28"),
            b')' => destination.push_str("%29"),
            b'#' => destination.push_str("%23"),
            b'?' => destination.push_str("%3F"),
            b'\\' => destination.push_str("%5C"),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b';' | b'/' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' => destination.push(char::from(byte)),
            other => destination.push_str(&format!("%{other:02X}")),
        }
    }
    format!("[{label}]({destination})")
}

/// The prompt with a link to each folder at its end, each followed by a space, and a space before the first unless the prompt is
/// empty or ends in one (`ensureLeadingBoundary`).
pub(crate) fn with_links(prompt: &str, folders: &[String]) -> String {
    let mut text = prompt.to_owned();
    for folder in folders {
        if !text.is_empty() && !text.ends_with(char::is_whitespace) {
            text.push(' ');
        }
        text.push_str(&file_link(folder));
        text.push(' ');
    }
    text
}

/// Puts the dropped folders in the prompt of `target`. A server on another computer cannot read them, and is told so.
pub(crate) fn add_folders(transport: &TransportHandle, state: Store<UiModel>, target: &DraftTarget, folders: Vec<String>) {
    if folders.is_empty() {
        return;
    }
    if !runtime::connected_to_loopback(transport) {
        notice::push(state, Level::Error, "Folders can't be dropped into remote environments", "");
        return;
    }
    match target.kind {
        DraftKind::Thread => {
            let prompt = with_links(&state.draft().peek(), &folders);
            crate::draft_storage::edit_thread_prompt(state, prompt);
        }
        DraftKind::Draft => {
            let Ok(environment) = t3_contracts::EnvironmentId::new(&target.environment) else { return };
            let key = (environment, target.local_id.clone());
            let prompt = with_links(state.new_thread_drafts().peek().get(&key).map_or("", String::as_str), &folders);
            crate::draft_storage::edit_draft_prompt(state, key, prompt);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_names_the_folder_and_escapes_what_would_end_the_link() {
        assert_eq!(file_link("/Users/me/code/app"), "[app](/Users/me/code/app)");
        assert_eq!(file_link("/Users/me/My Project (1)/a#b?c"), "[a#b?c](/Users/me/My%20Project%20%281%29/a%23b%3Fc)");
        assert_eq!(file_link("/tmp/odd[name]"), "[odd\\[name\\]](/tmp/odd%5Bname%5D)");
        assert_eq!(file_link(r"C:\Users\me\app"), "[app](C:%5CUsers%5Cme%5Capp)");
        assert_eq!(file_link("/tmp/\u{e9}t\u{e9}"), "[\u{e9}t\u{e9}](/tmp/%C3%A9t%C3%A9)");
    }

    #[test]
    fn links_follow_the_prompt_with_a_space_between_and_one_after() {
        let folders = ["/a/one".to_owned(), "/a/two".to_owned()];
        assert_eq!(with_links("", &folders), "[one](/a/one) [two](/a/two) ");
        assert_eq!(with_links("look at", &folders[..1]), "look at [one](/a/one) ");
        assert_eq!(with_links("look at ", &folders[..1]), "look at [one](/a/one) ");
        assert_eq!(with_links("x\n", &folders[..1]), "x\n[one](/a/one) ");
    }
}
