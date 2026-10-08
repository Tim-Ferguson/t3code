//! ThemeEditorPanel save candidates. Storage and activation settle in the App-owned writer.
use super::{Appearance, Catalog, Colors, Definition, library};
use serde_json::json;
use std::collections::BTreeMap;
#[derive(Clone, Debug)]
pub struct Draft {
    pub editing_id: Option<String>,
    pub name: String,
    pub appearance: Appearance,
    pub colors: BTreeMap<Appearance, Colors>,
    pub advanced: bool,
}
#[derive(Clone, Debug)]
pub struct Save {
    pub theme: Definition,
    pub created: bool,
    pub merged_appearance: Option<Appearance>,
    pub merge_target: Option<Definition>,
    pub retired: Option<Definition>,
}
fn trim(value: &str) -> &str {
    value.trim_matches(|c:char|matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
pub fn merge_target<'a>(
    catalog: &'a Catalog,
    name: &str,
    editing_id: Option<&str>,
) -> Option<&'a Definition> {
    let name = trim(name).to_lowercase();
    if name.is_empty() {
        return None;
    }
    let id = library::id_from_name(&name);
    catalog.custom.iter().find(|t| {
        Some(t.id.as_str()) != editing_id && (t.id == id || trim(&t.label).to_lowercase() == name)
    })
}
fn palette(draft: &Draft, mode: Appearance) -> Result<&Colors, String> {
    draft.colors.get(&mode).ok_or_else(|| {
        format!(
            "The draft has no {} palette. Reopen the editor and try again.",
            mode.key()
        )
    })
}
pub fn save(catalog: &Catalog, draft: &Draft) -> Result<Save, String> {
    if trim(&draft.name).is_empty() {
        return Err("Name your theme first.".into());
    }
    let editing = draft
        .editing_id
        .as_deref()
        .and_then(|id| catalog.custom.iter().find(|t| t.id == id));
    let target = merge_target(catalog, &draft.name, editing.map(|t| t.id.as_str()));
    let mut file = json!({"version":1,"name":draft.name,"appearance":draft.appearance,"colors":palette(draft,draft.appearance)?});
    let mut merged_appearance = None;
    let mut retired = None;
    if let Some(target) = target {
        let edited_modes = editing
            .map(|t| t.modes())
            .unwrap_or_else(|| vec![draft.appearance]);
        let collision = edited_modes
            .iter()
            .find(|mode| target.colors(**mode).is_some());
        if let Some(mode) = collision {
            return Err(if editing.is_some() {
                format!(
                    "“{}” already has a {} palette. Pick another name.",
                    target.label,
                    mode.key()
                )
            } else {
                format!(
                    "“{}” already has light and dark palettes. Pick another name.",
                    target.label
                )
            });
        }
        merged_appearance = edited_modes.first().copied();
        let mut variants = target.variants.clone().unwrap_or_default();
        for mode in edited_modes {
            variants.insert(mode, palette(draft, mode)?.clone());
        }
        file = json!({"version":1,"id":target.id,"name":target.label,"appearance":target.appearance,"colors":target.colors,"variants":variants});
        if target.managed == Some(true) && !draft.advanced {
            file["managed"] = json!(true);
        }
        if let Some(collection) = &target.collection {
            file["collection"] = collection.clone();
        }
        retired = editing.cloned();
    } else if let Some(editing) = editing {
        file = json!({"version":1,"id":editing.id,"name":draft.name,"appearance":editing.appearance,"colors":palette(draft,editing.appearance)?});
        if editing.modes().len() > 1 {
            let other = if editing.appearance == Appearance::Light {
                Appearance::Dark
            } else {
                Appearance::Light
            };
            file["variants"] = json!({other.key():palette(draft,other)?});
        }
        if !draft.advanced {
            file["managed"] = json!(true);
        }
        if let Some(collection) = &editing.collection {
            file["collection"] = collection.clone();
        }
    } else if !draft.advanced {
        file["managed"] = json!(true);
    }
    Ok(Save {
        theme: library::import(catalog, &file)?,
        created: editing.is_none() && target.is_none(),
        merged_appearance,
        merge_target: target.cloned(),
        retired,
    })
}
