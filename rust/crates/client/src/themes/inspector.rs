//! Theme token dependency policy. Color equality alone never proves dependency.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::LazyLock};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaintKind {
    Background,
    Border,
    Foreground,
}
impl PaintKind {
    pub const ORDER: [Self; 3] = [Self::Background, Self::Border, Self::Foreground];
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paint {
    pub background: String,
    pub border: String,
    pub foreground: String,
}
impl Paint {
    fn value(&self, kind: PaintKind) -> &str {
        match kind {
            PaintKind::Background => &self.background,
            PaintKind::Border => &self.border,
            PaintKind::Foreground => &self.foreground,
        }
    }
}
pub fn changed(before: &Paint, after: &Paint) -> Vec<PaintKind> {
    PaintKind::ORDER
        .into_iter()
        .filter(|&kind| before.value(kind) != after.value(kind))
        .collect()
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Family {
    pub id: String,
    pub label: String,
    pub role: String,
    pub roles: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub title: String,
    pub families: Vec<Family>,
}
#[derive(Deserialize)]
struct Data {
    utilities: BTreeMap<String, String>,
    groups: Vec<Group>,
    labels: BTreeMap<String, String>,
}
static DATA: LazyLock<Data> = LazyLock::new(|| {
    serde_json::from_str(include_str!("inspector-data.json")).expect("original inspector metadata")
});
pub fn groups() -> &'static [Group] {
    &DATA.groups
}
pub fn family(role: &str) -> Option<&'static Family> {
    groups()
        .iter()
        .flat_map(|g| &g.families)
        .find(|family| family.roles.iter().any(|r| r == role))
}
pub fn label(role: &str) -> String {
    DATA.labels.get(role).cloned().unwrap_or_else(|| {
        let mut value = String::new();
        for (index, ch) in role.chars().enumerate() {
            if ch.is_ascii_uppercase() {
                value.push(' ')
            }
            if index == 0 {
                value.extend(ch.to_uppercase())
            } else {
                value.push(ch)
            }
        }
        value
    })
}
pub fn utility(class: &str, kind: PaintKind) -> Option<&'static str> {
    if class.contains(':') {
        return None;
    }
    let prefixes: &[&str] = match kind {
        PaintKind::Background => &["bg-"],
        PaintKind::Border => &["border-", "outline-", "ring-"],
        PaintKind::Foreground => &["text-", "caret-", "fill-", "stroke-"],
    };
    for prefix in prefixes {
        if let Some(color) = class.strip_prefix(prefix) {
            return DATA
                .utilities
                .get(color.split('/').next().unwrap_or(""))
                .map(String::as_str);
        }
    }
    None
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Bounds {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
    pub right: f64,
    pub bottom: f64,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rectangle {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub radius: f64,
}
pub fn rectangle(bounds: Bounds, viewport: [f64; 2], radius: f64) -> Option<Rectangle> {
    if bounds.width <= 0.
        || bounds.height <= 0.
        || bounds.right < 0.
        || bounds.bottom < 0.
        || bounds.left > viewport[0]
        || bounds.top > viewport[1]
    {
        return None;
    }
    Some(Rectangle {
        x: bounds.left - 5.,
        y: bounds.top - 5.,
        width: bounds.width + 10.,
        height: bounds.height + 10.,
        radius: (radius + 5.).clamp(7., 18.),
    })
}

/// Guided colors highlight matching literal roles; advanced selections highlight
/// the explicitly managed family rather than unrelated equal-color tokens.
pub fn highlight_roles(
    selected: Option<&str>,
    advanced: bool,
    colors: &super::Colors,
    roles: &[String],
) -> Vec<String> {
    let Some(selected) = selected else {
        return vec![];
    };
    if advanced {
        return family(selected)
            .map(|family| family.roles.clone())
            .unwrap_or_else(|| vec![selected.into()]);
    }
    if ["canvas", "accent"].contains(&selected) {
        let Some(value) = colors.get(selected) else {
            return vec![];
        };
        let value = super::color::trim(value).to_lowercase();
        roles
            .iter()
            .filter(|role| {
                colors
                    .get(*role)
                    .is_some_and(|color| super::color::trim(color).to_lowercase() == value)
            })
            .cloned()
            .collect()
    } else {
        vec![selected.into()]
    }
}

pub fn filtered_groups(query: &str) -> Vec<Group> {
    let query = super::color::trim(query).to_lowercase();
    groups()
        .iter()
        .filter_map(|group| {
            let families: Vec<_> = group
                .families
                .iter()
                .filter(|family| {
                    query.is_empty()
                        || std::iter::once(family.label.clone())
                            .chain(family.roles.iter().map(|role| label(role)))
                            .collect::<Vec<_>>()
                            .join(" ")
                            .to_lowercase()
                            .contains(&query)
                })
                .cloned()
                .collect();
            (!families.is_empty()).then(|| Group {
                id: group.id.clone(),
                title: group.title.clone(),
                families,
            })
        })
        .collect()
}
