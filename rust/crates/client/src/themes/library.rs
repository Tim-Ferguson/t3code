//! Lossless source-key recovery and pure theme-library mutation candidates.
//! Candidates become visible only after the owning serialized storage writer acknowledges them.
use super::{Appearance, Catalog, Colors, Definition, color};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, PartialEq)]
pub enum Library {
    Ready {
        stored: Vec<Value>,
        themes: Vec<Definition>,
    },
    Unavailable {
        reason: &'static str,
    },
}
fn trim(value: &str) -> &str {
    value.trim_matches(|c:char|matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
fn label(value: &Value) -> Option<String> {
    let text = trim(value.as_str()?);
    (1..=48)
        .contains(&text.encode_utf16().count())
        .then(|| text.to_owned())
}
pub fn valid_id(id: &str) -> bool {
    (1..=48).contains(&id.len())
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && id.as_bytes()[0] != b'-'
}
fn appearance(value: &Value) -> Option<Appearance> {
    match value.as_str()? {
        "light" => Some(Appearance::Light),
        "dark" => Some(Appearance::Dark),
        _ => None,
    }
}
fn collection(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?;
    if !(1..=128).contains(&id.len())
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".:-".contains(&c))
    {
        return None;
    }
    Some(json!({"id":id,"label":label(object.get("label")?)?}))
}
pub fn defaults(catalog: &Catalog, mode: Appearance) -> Colors {
    catalog
        .data
        .builtin
        .iter()
        .find(|t| t.id == "t3-chat")
        .and_then(|t| t.colors(mode))
        .expect("source flagship theme")
        .clone()
}
pub fn lenient_colors(catalog: &Catalog, value: &Value, mode: Appearance) -> Option<Colors> {
    let object = value.as_object()?;
    let mut colors = defaults(catalog, mode);
    for (role, value) in object {
        if catalog.data.roles.contains(role) {
            if let Some(color) = value.as_str().and_then(color::canonical) {
                colors.insert(role.clone(), color);
            }
        }
    }
    Some(colors)
}
pub fn stored_theme(catalog: &Catalog, value: &Value) -> Option<Definition> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?;
    if !valid_id(id) || catalog.data.reserved.iter().any(|v| v == id) {
        return None;
    }
    let mode = appearance(object.get("appearance")?)?;
    let mut variants = BTreeMap::new();
    if let Some(value) = object.get("variants") {
        for (key, value) in value.as_object()? {
            let variant = appearance(&Value::String(key.clone()))?;
            if variant != mode {
                variants.insert(variant, lenient_colors(catalog, value, variant)?);
            }
        }
    }
    Some(Definition {
        id: id.to_owned(),
        label: label(object.get("label")?)?,
        appearance: mode,
        colors: lenient_colors(catalog, object.get("colors")?, mode)?,
        variants: (!variants.is_empty()).then_some(variants),
        collection: object.get("collection").and_then(collection),
        sidebar_artwork: None,
        managed: (object.get("managed") == Some(&Value::Bool(true))).then_some(true),
    })
}
pub fn stored_themes(catalog: &Catalog, values: &[Value]) -> Vec<Definition> {
    let mut ids = BTreeSet::new();
    values
        .iter()
        .filter_map(|v| stored_theme(catalog, v))
        .filter(|v| ids.insert(v.id.clone()))
        .collect()
}
impl Library {
    pub fn read(catalog: &Catalog, raw: Result<Option<&str>, ()>) -> Self {
        match raw {
            Err(()) => Self::Unavailable {
                reason: "storage-unavailable",
            },
            Ok(None | Some("")) => Self::Ready {
                stored: vec![],
                themes: vec![],
            },
            Ok(Some(text)) => match serde_json::from_str::<Value>(text) {
                Ok(Value::Array(stored)) => Self::Ready {
                    themes: stored_themes(catalog, &stored),
                    stored,
                },
                _ => Self::Unavailable {
                    reason: "malformed",
                },
            },
        }
    }
    pub fn ready(&self) -> Result<(&[Value], &[Definition]), String> {
        match self {
            Self::Ready { stored, themes } => Ok((stored, themes)),
            Self::Unavailable { .. } => Err(format!(
                "Failed to read the theme library from {}.",
                super::CUSTOM_KEY
            )),
        }
    }
    fn candidate(catalog: &Catalog, stored: Vec<Value>) -> Self {
        Self::Ready {
            themes: stored_themes(catalog, &stored),
            stored,
        }
    }
    pub fn bytes(&self) -> Result<String, String> {
        Ok(serde_json::to_string(self.ready()?.0).unwrap())
    }
    pub fn install(&self, catalog: &Catalog, theme: &Definition) -> Result<Self, String> {
        reserved(catalog, &theme.id)?;
        let (stored, _) = self.ready()?;
        if catalog.data.builtin.iter().any(|t| t.id == theme.id)
            || stored.iter().any(|t| has_id(t, &theme.id))
        {
            return Err(format!(
                "A theme named \"{}\" is already installed.",
                theme.label
            ));
        }
        let value = canonical_value(catalog, theme)?;
        let mut next = stored.to_vec();
        next.push(value);
        Ok(Self::candidate(catalog, next))
    }
    pub fn update(&self, catalog: &Catalog, theme: &Definition) -> Result<Self, String> {
        reserved(catalog, &theme.id)?;
        let (stored, themes) = self.ready()?;
        if !themes.iter().any(|t| t.id == theme.id) {
            return Err(format!("The theme \"{}\" is not installed.", theme.label));
        }
        let value = canonical_value(catalog, theme)?;
        let mut replaced = false;
        let mut next = vec![];
        for row in stored {
            if !has_id(row, &theme.id) {
                next.push(row.clone());
            } else if !replaced {
                next.push(value.clone());
                replaced = true;
            }
        }
        Ok(Self::candidate(catalog, next))
    }
    /// None means the source performs no write, including removal of malformed-only ids.
    pub fn remove(&self, catalog: &Catalog, ids: &[String]) -> Result<Option<Self>, String> {
        if ids.is_empty() {
            return Ok(None);
        }
        let (stored, themes) = self.ready()?;
        if !themes.iter().any(|t| ids.contains(&t.id)) {
            return Ok(None);
        }
        Ok(Some(Self::candidate(
            catalog,
            stored
                .iter()
                .filter(|v| {
                    !v.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| ids.iter().any(|i| i == id))
                })
                .cloned()
                .collect(),
        )))
    }
    pub fn replace_collection(
        &self,
        catalog: &Catalog,
        id: &str,
        values: &[Value],
        expected: Option<&[Value]>,
    ) -> Result<Self, String> {
        if values.is_empty() {
            return Err("A theme collection cannot be empty.".into());
        }
        let mut ids = BTreeSet::new();
        let mut replacement = vec![];
        for value in values {
            let theme = stored_theme(catalog, value).filter(|t| {
                t.collection
                    .as_ref()
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
                    == Some(id)
            });
            let Some(theme) = theme else {
                return Err("That theme collection is invalid.".into());
            };
            if !ids.insert(theme.id.clone()) {
                return Err("That theme collection is invalid.".into());
            }
            replacement.push(theme);
        }
        let (stored, themes) = self.ready()?;
        if let Some(expected) = expected {
            let current: Vec<Value> = themes
                .iter()
                .filter(|t| {
                    t.collection
                        .as_ref()
                        .and_then(|v| v.get("id"))
                        .and_then(Value::as_str)
                        == Some(id)
                })
                .map(|t| definition_value(catalog, t))
                .collect();
            if serde_json::to_string(&current).unwrap() != serde_json::to_string(expected).unwrap()
            {
                return Err(
                    "Your installed themes changed while this package was downloading. Try again."
                        .into(),
                );
            }
        }
        for theme in &replacement {
            if catalog.data.reserved.contains(&theme.id)
                || catalog.data.builtin.iter().any(|t| t.id == theme.id)
                || stored
                    .iter()
                    .any(|v| !has_collection(v, id) && has_id(v, &theme.id))
            {
                return Err(format!(
                    "A theme named \"{}\" is already installed.",
                    theme.label
                ));
            }
        }
        let replacement: Vec<Value> = replacement
            .iter()
            .map(|t| definition_value(catalog, t))
            .collect();
        let mut next = vec![];
        let mut inserted = false;
        for row in stored {
            if !has_collection(row, id) {
                next.push(row.clone())
            } else if !inserted {
                next.extend(replacement.clone());
                inserted = true;
            }
        }
        if !inserted {
            next.extend(replacement)
        }
        Ok(Self::candidate(catalog, next))
    }
}
fn has_id(value: &Value, id: &str) -> bool {
    value
        .as_object()
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        == Some(id)
}
fn has_collection(value: &Value, id: &str) -> bool {
    value
        .as_object()
        .and_then(|o| o.get("collection"))
        .and_then(Value::as_object)
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        == Some(id)
}
fn reserved(catalog: &Catalog, id: &str) -> Result<(), String> {
    if catalog.data.reserved.iter().any(|v| v == id) {
        Err(format!("The theme id \"{id}\" is reserved."))
    } else {
        Ok(())
    }
}
pub fn id_from_name(name: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for c in trim(name).to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            if separator && !result.is_empty() {
                result.push('-')
            }
            result.push(c);
            separator = false;
        } else {
            separator = true;
        }
    }
    result.truncate(result.len().min(48));
    if result.is_empty() {
        "custom-theme".into()
    } else {
        result
    }
}
fn overrides(catalog: &Catalog, value: &Value) -> Result<Colors, String> {
    let object = value.as_object().ok_or("Theme colors must be objects.")?;
    let mut colors = Colors::new();
    for (role, value) in object {
        if !catalog.data.roles.contains(role) {
            return Err(format!("\"{role}\" is not a supported theme color role."));
        }
        let color = value.as_str().and_then(color::canonical).ok_or_else(|| {
            format!(
                "The color for \"{role}\" must be a literal CSS color such as oklch(0.62 0.2 280)."
            )
        })?;
        colors.insert(role.clone(), color);
    }
    if colors.is_empty() {
        return Err("Add at least one color role to the theme file.".into());
    }
    Ok(colors)
}
pub fn import(catalog: &Catalog, value: &Value) -> Result<Definition, String> {
    let object = value
        .as_object()
        .ok_or("Theme files must contain a JSON object.")?;
    if object.get("version").and_then(Value::as_f64) != Some(1.) {
        return Err("This theme file uses an unsupported version. Expected 1.".into());
    }
    let name = object
        .get("name")
        .and_then(label)
        .ok_or("Theme files need a name (48 characters or fewer).")?;
    let mode = object
        .get("appearance")
        .and_then(appearance)
        .ok_or("Theme files need an appearance of \"light\" or \"dark\".")?;
    let raw = object
        .get("colors")
        .filter(|v| v.is_object())
        .ok_or("Theme files need a colors object.")?;
    let id = match object.get("id") {
        None => id_from_name(&name),
        Some(v) => v
            .as_str()
            .filter(|v| valid_id(v))
            .ok_or("Theme ids may only contain lowercase letters, numbers, and hyphens.")?
            .to_owned(),
    };
    reserved(catalog, &id)?;
    let mut colors = defaults(catalog, mode);
    colors.extend(overrides(catalog, raw)?);
    let group = match object.get("collection") {
        None => None,
        Some(v) => Some(collection(v).ok_or("Theme collections need a valid id and label.")?),
    };
    let mut variants = BTreeMap::new();
    if let Some(raw) = object.get("variants") {
        for (key, raw) in raw.as_object().ok_or("Theme variants must be an object.")? {
            let variant = appearance(&Value::String(key.clone()))
                .ok_or("Theme variants may only be named \"light\" or \"dark\".")?;
            if variant == mode {
                return Err(format!(
                    "Theme variants must not repeat the base appearance \"{}\".",
                    mode.key()
                ));
            }
            let mut colors = defaults(catalog, variant);
            colors.extend(overrides(catalog, raw)?);
            variants.insert(variant, colors);
        }
    }
    Ok(Definition {
        id,
        label: name,
        appearance: mode,
        colors,
        variants: (!variants.is_empty()).then_some(variants),
        collection: group,
        sidebar_artwork: None,
        managed: (object.get("managed") == Some(&Value::Bool(true))).then_some(true),
    })
}
fn ordered_colors(catalog: &Catalog, colors: &Colors) -> Value {
    Value::Object(
        catalog
            .data
            .roles
            .iter()
            .filter_map(|role| {
                colors
                    .get(role)
                    .map(|color| (role.clone(), Value::String(color.clone())))
            })
            .collect(),
    )
}
pub fn definition_value(catalog: &Catalog, theme: &Definition) -> Value {
    let mut object = Map::new();
    object.insert("id".into(), json!(theme.id));
    object.insert("label".into(), json!(theme.label));
    object.insert("appearance".into(), json!(theme.appearance));
    object.insert("colors".into(), ordered_colors(catalog, &theme.colors));
    if let Some(variants) = &theme.variants {
        object.insert(
            "variants".into(),
            Value::Object(
                variants
                    .iter()
                    .map(|(mode, colors)| (mode.key().into(), ordered_colors(catalog, colors)))
                    .collect(),
            ),
        );
    }
    if let Some(v) = &theme.collection {
        object.insert("collection".into(), v.clone());
    }
    if let Some(v) = theme.sidebar_artwork {
        object.insert("sidebarArtwork".into(), json!(v));
    }
    if let Some(v) = theme.managed {
        object.insert("managed".into(), json!(v));
    }
    Value::Object(object)
}
fn canonical_value(catalog: &Catalog, theme: &Definition) -> Result<Value, String> {
    let mut result = theme.clone();
    let canonical = |colors: &Colors| -> Result<Colors, String> {
        catalog.data.roles.iter().map(|role|Ok((role.clone(),colors.get(role).and_then(|v|color::canonical(v)).ok_or_else(||format!("The color for \"{role}\" must be a literal CSS color such as oklch(0.62 0.2 280)."))?))).collect()
    };
    result.colors = canonical(&theme.colors)?;
    result.variants = theme
        .variants
        .as_ref()
        .map(|variants| {
            variants
                .iter()
                .map(|(mode, colors)| Ok((*mode, canonical(colors)?)))
                .collect::<Result<BTreeMap<_, _>, String>>()
        })
        .transpose()?;
    Ok(definition_value(catalog, &result))
}
pub fn export(catalog: &Catalog, theme: &Definition) -> Result<String, String> {
    let canonical = canonical_value(catalog, theme)?;
    let mut file = Map::new();
    file.insert("version".into(), json!(1));
    for (target, source) in [
        ("id", "id"),
        ("name", "label"),
        ("appearance", "appearance"),
        ("colors", "colors"),
        ("variants", "variants"),
        ("collection", "collection"),
        ("managed", "managed"),
    ] {
        if let Some(v) = canonical.get(source) {
            if source != "managed" || v == &Value::Bool(true) {
                file.insert(target.into(), v.clone());
            }
        }
    }
    Ok(serde_json::to_string_pretty(&Value::Object(file)).unwrap() + "\n")
}
