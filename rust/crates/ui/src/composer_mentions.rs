//! The files a prompt names, drawn as chips above the message box (the chips of `ComposerPromptEditorTiptap.tsx`, with `FileTagChip.tsx`'s
//! icon and label): choosing an `@` path writes the canonical file link into the prompt, and each link (or bare `@path`) in the
//! text shows here as a chip with a button that takes it out of the text. The same chip is what a sent message draws for a file link
//! (`workspace_files::FileLink`). Which words are mentions is `t3_client::composer_mentions`.
//!
//! The icon is one glyph for a file and one for a folder; the original draws the Pierre icon set by extension.
//! TODO(original editor chips): in the original the chip stands in the text where the link is; the message box here is a text field, so the link
//! is shown in it as written and the chip above it.
use dioxus::prelude::*;
use t3_client::composer_mentions as logic;

/// The glyph of a file chip: a folder for a name the original takes for a folder, else a page.
pub(crate) fn icon(path: &str) -> &'static str {
    if logic::is_folder(path) { "\u{1F4C1}" } else { "\u{1F4C4}" }
}

/// The chips for the files `prompt` names. `on_edit` is told the prompt without a chip the person removes. Nothing is drawn when none is named.
#[component]
pub(crate) fn Tray(prompt: String, on_edit: EventHandler<String>) -> Element {
    let found = logic::mentions(&prompt);
    if found.is_empty() {
        return rsx! {};
    }
    rsx! { ul { class: "context-chips", "aria-label": "Mentioned files",
        for (index, mention) in found.into_iter().enumerate() {
            {
                let label = mention.path.rsplit(['/', '\\']).next().unwrap_or(&mention.path).to_owned();
                let glyph = icon(&mention.path);
                let (path, remove_label) = (mention.path.clone(), format!("Remove {label}"));
                let next = logic::remove(&prompt, &mention);
                rsx! { li { key: "{index}:{mention.start}", class: "context-chip mention-chip", title: "{path}", "data-mention-path": "{path}",
                    span { class: "file-chip-icon", "aria-hidden": "true", "{glyph}" }
                    span { class: "context-chip-label", "{label}" }
                    button { r#type: "button", class: "context-chip-remove", "aria-label": "{remove_label}", onclick: move |_| on_edit.call(next.clone()), "\u{d7}" }
                } }
            }
        }
    } }
}
