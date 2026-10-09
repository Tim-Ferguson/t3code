//! Which editors a thread can be opened in, and which one the shortcut opens. Port of
//! `usePreferredEditor` (`editorPreferences.ts`) and `editorLabels.ts`; the table
//! (`editors.json`) is written from the original's `EDITORS` by
//! `rust/tools/generate_client_editor_fixtures.mjs`.
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Debug, Deserialize)]
pub struct Editor {
    pub id: String,
    pub label: String,
}
static EDITORS: LazyLock<Vec<Editor>> = LazyLock::new(|| serde_json::from_str(include_str!("editors.json")).expect("editor table"));

/// The browser's key for the editor last chosen.
pub const LAST_EDITOR_KEY: &str = "t3code:last-editor";
/// The file manager is not an editor: it reveals the folder, and a server says whether it can.
pub const FILE_MANAGER: &str = "file-manager";

/// What the server offers, in the original's order.
pub fn offered(available: &[String]) -> Vec<&'static Editor> {
    EDITORS.iter().filter(|editor| available.contains(&editor.id)).collect()
}
/// The editor the favorite shortcut and the main button use: the last one chosen when it is still
/// offered, else the first offered.
pub fn preferred(available: &[String], last: Option<&str>) -> Option<String> {
    last.filter(|last| available.iter().any(|id| id == last)).map(str::to_owned).or_else(|| EDITORS.iter().find(|editor| available.contains(&editor.id)).map(|editor| editor.id.clone()))
}
/// `openInEditorMenuLabel`.
pub fn menu_label(editor: Option<&str>) -> String {
    match editor {
        None | Some(FILE_MANAGER) => "Open in editor".to_owned(),
        Some(id) => format!("Open in {}", EDITORS.iter().find(|editor| editor.id == id).map_or("Editor", |editor| editor.label.as_str())),
    }
}
/// A menu entry's label; the file manager is named for what it does.
pub fn entry_label(editor: &Editor) -> String {
    if editor.id == FILE_MANAGER { "Reveal in file manager".to_owned() } else { editor.label.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn the_preferred_editor_and_its_label_match_the_original() {
        let mut checked = 0;
        for line in include_str!("../tests/fixtures/editors.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let available: Vec<String> = row["available"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect();
            let chosen = preferred(&available, row["last"].as_str());
            assert_eq!(chosen.as_deref(), row["preferred"].as_str(), "{row}");
            assert_eq!(menu_label(chosen.as_deref()), row["label"].as_str().unwrap(), "{row}");
            checked += 1;
        }
        assert!(checked >= 300);
    }

    #[test]
    fn only_offered_editors_are_listed_in_the_originals_order() {
        let available = vec!["zed".to_owned(), "file-manager".to_owned(), "cursor".to_owned(), "mystery".to_owned()];
        let ids: Vec<&str> = offered(&available).iter().map(|editor| editor.id.as_str()).collect();
        assert_eq!(ids, ["cursor", "zed", "file-manager"]);
        assert_eq!(entry_label(offered(&available)[2]), "Reveal in file manager");
    }
}
