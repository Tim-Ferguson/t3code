//! Original theme catalog and storage policy. Palette data comes from the source shared package.
pub mod color;
pub mod editor;
pub mod environment;
pub mod import;
mod jsonc;
pub mod library;
pub mod openvsx;
pub mod storage;
pub mod vivid;
pub mod vscode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
pub const THEME_KEY: &str = "t3code:theme";
pub const MODE_KEY: &str = "t3code:theme-appearance-mode";
pub const FOLLOW_KEY: &str = "t3code:theme-follow-system";
pub const HALVES_KEY: &str = "t3code:theme-halves:v1";
pub const CUSTOM_KEY: &str = "t3code:themes:v1";
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Light,
    Dark,
}
impl Appearance {
    pub fn key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Light,
    Dark,
    #[default]
    System,
}
impl From<Appearance> for Mode {
    fn from(mode: Appearance) -> Self {
        match mode {
            Appearance::Light => Self::Light,
            Appearance::Dark => Self::Dark,
        }
    }
}
impl Mode {
    pub fn key(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}
pub type Colors = BTreeMap<String, String>;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Definition {
    pub id: String,
    pub label: String,
    pub appearance: Appearance,
    pub colors: Colors,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variants: Option<BTreeMap<Appearance, Colors>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidebar_artwork: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<bool>,
}
impl Definition {
    pub fn colors(&self, appearance: Appearance) -> Option<&Colors> {
        if appearance == self.appearance {
            Some(&self.colors)
        } else {
            self.variants.as_ref()?.get(&appearance)
        }
    }
    pub fn modes(&self) -> Vec<Appearance> {
        [Appearance::Light, Appearance::Dark]
            .into_iter()
            .filter(|&mode| self.colors(mode).is_some())
            .collect()
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct Builtins {
    pub builtin: Vec<Definition>,
    pub standard: BTreeMap<Appearance, Colors>,
    pub roles: Vec<String>,
    pub reserved: Vec<String>,
    pub variables: BTreeMap<String, String>,
}
impl Default for Builtins {
    fn default() -> Self {
        serde_json::from_str(include_str!("builtin.json")).expect("original builtin palette data")
    }
}
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub data: Builtins,
    pub custom: Vec<Definition>,
    pub environment: Vec<Definition>,
}
pub fn normalize_id(id: &str) -> &str {
    match id {
        "t3-chat-dark" => "t3-chat",
        "t3-grove" => "grove",
        "t3-ocean" => "ocean",
        "t3-ember" => "ember",
        "t3-iris" => "iris",
        _ => id,
    }
}
pub fn canonical_preference(id: &str) -> &str {
    if id == "t3-chat-dark" {
        id
    } else {
        normalize_id(id)
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Halves {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark: Option<String>,
}
impl Halves {
    pub fn get(&self, appearance: Appearance) -> Option<&str> {
        match appearance {
            Appearance::Light => self.light.as_deref(),
            Appearance::Dark => self.dark.as_deref(),
        }
    }
    pub fn empty(&self) -> bool {
        self.light.is_none() && self.dark.is_none()
    }
}
impl Catalog {
    pub fn definition(&self, preference: &str) -> Option<&Definition> {
        let id = normalize_id(preference);
        self.data
            .builtin
            .iter()
            .chain(&self.custom)
            .chain(&self.environment)
            .find(|theme| theme.id == id)
    }
    pub fn known(&self, preference: &str) -> bool {
        Mode::parse(preference).is_some() || self.definition(preference).is_some()
    }
    pub fn preference_mode(&self, preference: &str) -> Option<Appearance> {
        match preference {
            "system" => None,
            "light" => Some(Appearance::Light),
            "dark" | "t3-chat-dark" => Some(Appearance::Dark),
            _ => self.definition(preference).map(|theme| theme.appearance),
        }
    }
    pub fn stored_preference(&self, raw: Option<&str>) -> String {
        raw.filter(|raw| self.known(raw))
            .map(canonical_preference)
            .unwrap_or("system")
            .into()
    }
    pub fn stored_mode(&self, theme: &str, mode: Option<&str>, follow: Option<&str>) -> Mode {
        if let Some(mode) = mode.and_then(Mode::parse) {
            return mode;
        }
        let follow = match follow {
            Some("true") => true,
            Some("false") => false,
            _ => theme == "system",
        };
        if follow {
            Mode::System
        } else {
            self.preference_mode(theme)
                .map(Mode::from)
                .unwrap_or(Mode::Light)
        }
    }
    pub fn parse_halves(&self, raw: Option<&str>) -> Option<Halves> {
        let value: Value = serde_json::from_str(raw?).ok()?;
        let object = value.as_object()?;
        let mut halves = Halves::default();
        for appearance in [Appearance::Light, Appearance::Dark] {
            let Some(id) = object.get(appearance.key()).and_then(Value::as_str) else {
                continue;
            };
            if let Some(theme) = self
                .definition(id)
                .filter(|theme| theme.colors(appearance).is_some())
            {
                match appearance {
                    Appearance::Light => halves.light = Some(theme.id.clone()),
                    Appearance::Dark => halves.dark = Some(theme.id.clone()),
                }
            }
        }
        (!halves.empty()).then_some(halves)
    }
    pub fn resolve(
        &self,
        theme: &str,
        system_dark: bool,
        follow: Option<bool>,
        mode: Option<Mode>,
        halves: Option<&Halves>,
    ) -> Appearance {
        let mode = mode.or_else(|| follow.unwrap_or(theme == "system").then_some(Mode::System));
        let appearance = match mode {
            Some(Mode::System) => Some(if system_dark {
                Appearance::Dark
            } else {
                Appearance::Light
            }),
            Some(Mode::Light) => Some(Appearance::Light),
            Some(Mode::Dark) => Some(Appearance::Dark),
            None => None,
        };
        if let Some(appearance) = appearance {
            if halves.and_then(|halves| halves.get(appearance)).is_some() {
                return appearance;
            }
            return self
                .definition(theme)
                .filter(|theme| theme.colors(appearance).is_none())
                .map(|theme| theme.appearance)
                .unwrap_or(appearance);
        }
        self.preference_mode(theme).unwrap_or(Appearance::Light)
    }
    pub fn desktop_mode(
        &self,
        theme: &str,
        follow: Option<bool>,
        mode: Option<Mode>,
        halves: Option<&Halves>,
    ) -> Mode {
        let mode = mode.or_else(|| follow.unwrap_or(theme == "system").then_some(Mode::System));
        match mode {
            Some(Mode::System) => self
                .definition(theme)
                .filter(|theme| {
                    [Appearance::Light, Appearance::Dark]
                        .into_iter()
                        .any(|appearance| {
                            halves.and_then(|halves| halves.get(appearance)).is_none()
                                && theme.colors(appearance).is_none()
                        })
                })
                .map(|theme| theme.appearance.into())
                .unwrap_or(Mode::System),
            Some(mode) => {
                let appearance = if mode == Mode::Light {
                    Appearance::Light
                } else {
                    Appearance::Dark
                };
                if halves.and_then(|halves| halves.get(appearance)).is_some() {
                    return mode;
                }
                self.definition(theme)
                    .filter(|theme| theme.colors(appearance).is_none())
                    .map(|theme| theme.appearance.into())
                    .unwrap_or(mode)
            }
            None => self
                .preference_mode(theme)
                .map(Mode::from)
                .unwrap_or(Mode::System),
        }
    }
    pub fn half<'a>(
        &self,
        theme: &'a str,
        halves: Option<&'a Halves>,
        appearance: Appearance,
    ) -> &'a str {
        halves
            .and_then(|halves| halves.get(appearance))
            .unwrap_or(theme)
    }
}
/// Raw reads deliberately retain unpublished IDs so changing one half cannot erase the other.
pub fn raw_halves(raw: Option<&str>) -> Halves {
    let value = raw.and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let get = |key: &str| {
        value
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    Halves {
        light: get("light"),
        dark: get("dark"),
    }
}
/// Same transparent-value and ECMAScript whitespace predicate as syncBrowserChromeTheme.
pub fn normalize_browser_color(value: &str) -> Option<&str> {
    let value=value.trim_matches(|c:char|matches!(c,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'));
    if value.is_empty()
        || ["transparent", "rgba(0, 0, 0, 0)", "rgba(0 0 0 / 0)"]
            .contains(&value.to_ascii_lowercase().as_str())
    {
        None
    } else {
        Some(value)
    }
}
