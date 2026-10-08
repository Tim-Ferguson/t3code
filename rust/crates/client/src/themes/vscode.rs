//! Original vscodeThemeImport.ts workbench conversion, pairing and collision policy.
use super::{Appearance, Catalog, Definition, color, library, vivid};
use regex::Regex;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::LazyLock,
};
#[derive(Clone, Copy)]
struct Rgba {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}
const BLACK: Rgba = Rgba {
    r: 0.,
    g: 0.,
    b: 0.,
    a: 1.,
};
static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?").unwrap()
});
fn parse_float(value: &str) -> Option<f64> {
    let value = color::trim(value);
    let number = NUMBER.find(value)?.as_str();
    let mut number = number.parse::<f64>().ok()?;
    if value.ends_with('%') {
        number /= 100.;
    }
    number.is_finite().then_some(number)
}
fn whitespace(c: char) -> bool {
    color::trim(&c.to_string()).is_empty()
}
fn gamma(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        libm::pow((value + 0.055) / 1.055, 2.4)
    }
}
fn encode(value: f64) -> f64 {
    let value = value.clamp(0., 1.);
    if value <= 0.0031308 {
        value * 12.92
    } else {
        1.055 * libm::pow(value, 1. / 2.4) - 0.055
    }
}
fn parse(value: &Value) -> Option<Rgba> {
    let value = color::trim(value.as_str()?);
    if value.starts_with("color(") {
        let body = value.strip_prefix("color(")?.strip_suffix(')')?;
        if body.contains(')') {
            return None;
        }
        let body = body.trim_start_matches(whitespace);
        let split = body.find(whitespace)?;
        let space = &body[..split];
        if !space.eq_ignore_ascii_case("srgb") && !space.eq_ignore_ascii_case("display-p3") {
            return None;
        }
        let mut parts = body[split..].trim_start_matches(whitespace).split('/');
        let channels: Vec<_> = color::trim(parts.next()?)
            .split(whitespace)
            .filter(|s| !s.is_empty())
            .map(parse_float)
            .collect();
        if channels.len() != 3 {
            return None;
        }
        let mut r = channels[0]?;
        let mut g = channels[1]?;
        let mut b = channels[2]?;
        let a = parts
            .next()
            .map(parse_float)
            .unwrap_or(Some(1.))?
            .clamp(0., 1.);
        if space.eq_ignore_ascii_case("display-p3") {
            let lr = gamma(r);
            let lg = gamma(g);
            let lb = gamma(b);
            r = encode(1.2249401762805 * lr - 0.2249401762805 * lg);
            g = encode(-0.042056961239 * lr + 1.042056961239 * lg);
            b = encode(-0.0196375547643 * lr - 0.0786360655012 * lg + 1.0982736202656 * lb);
        }
        return Some(Rgba {
            r: r * 255.,
            g: g * 255.,
            b: b * 255.,
            a,
        });
    }
    let hex = value.strip_prefix('#').unwrap_or(value);
    if !matches!(hex.len(), 3 | 4 | 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |index| {
        if hex.len() <= 4 {
            u8::from_str_radix(&hex[index..index + 1], 16).unwrap() as f64 * 17.
        } else {
            u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap() as f64
        }
    };
    Some(Rgba {
        r: channel(0),
        g: channel(1),
        b: channel(2),
        a: if matches!(hex.len(), 4 | 8) {
            channel(3) / 255.
        } else {
            1.
        },
    })
}
fn hex(color: Rgba) -> String {
    let channel = |v: f64| {
        if v.is_nan() {
            "NaN".to_owned()
        } else {
            format!("{:02x}", ((v + 0.5).floor().clamp(0., 255.)) as u8)
        }
    };
    format!(
        "#{}{}{}",
        channel(color.r),
        channel(color.g),
        channel(color.b)
    )
}
fn flatten(color: Rgba, base: Rgba) -> String {
    if color.a >= 1. {
        hex(color)
    } else {
        hex(Rgba {
            r: color.r * color.a + base.r * (1. - color.a),
            g: color.g * color.a + base.g * (1. - color.a),
            b: color.b * color.a + base.b * (1. - color.a),
            a: 1.,
        })
    }
}
fn luminance(color: Rgba) -> f64 {
    let channel = |v: f64| {
        let v = v / 255.;
        if v <= 0.03928 {
            v / 12.92
        } else {
            libm::pow((v + 0.055) / 1.055, 2.4)
        }
    };
    0.2126 * channel(color.r) + 0.7152 * channel(color.g) + 0.0722 * channel(color.b)
}
fn rgb(value: &str) -> Rgba {
    parse(&json!(color::hex(value).unwrap_or_else(|| value.into()))).unwrap_or(BLACK)
}
fn contrast(first: Rgba, second: Rgba) -> f64 {
    let a = luminance(first);
    let b = luminance(second);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}
fn apart(first: &str, second: &str) -> bool {
    contrast(rgb(first), rgb(second)) >= 1.1
}
fn pick(colors: &Value, keys: &[&str]) -> Option<Rgba> {
    keys.iter().find_map(|key| parse(&colors[*key]))
}
fn solid(colors: &Value, base: Rgba, keys: &[&str]) -> Option<String> {
    pick(colors, keys).map(|v| flatten(v, base))
}
fn readable(colors: &Value, surface: &str, fallback: &str, keys: &[&str]) -> String {
    let base = rgb(surface);
    let readable = |candidate: &str| contrast(rgb(candidate), base) >= 4.5;
    if let Some(value) = solid(colors, base, keys).filter(|v| readable(v)) {
        return value;
    }
    if readable(fallback) {
        return fallback.into();
    }
    if luminance(base) < 0.179 {
        "#ffffff".into()
    } else {
        "#000000".into()
    }
}
pub fn is_file(value: &Value) -> bool {
    value.is_object()
        && value["version"].as_f64() != Some(1.)
        && ((value["colors"]
            .as_object()
            .is_some_and(|c| c.keys().any(|k| k.contains('.'))))
            || value["tokenColors"].is_array())
}
pub fn humanize_name(raw: &str) -> String {
    let raw = color::trim(raw);
    if raw.chars().any(whitespace) || !raw.contains(['-', '_', '.']) {
        return raw.into();
    }
    raw.split(['-', '_', '.'])
        .filter(|s| !s.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            let first = chars.next().unwrap();
            first.to_uppercase().collect::<String>() + chars.as_str()
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn slice_name(value: &str) -> String {
    let mut length = 0;
    value
        .chars()
        .take_while(|c| {
            length += c.len_utf16();
            length <= 48
        })
        .collect()
}
pub fn import(catalog: &Catalog, value: &Value) -> Result<Definition, String> {
    if !value.is_object() {
        return Err("Theme files must contain a JSON object.".into());
    }
    let colors = &value["colors"];
    let canvas=pick(colors,&["editor.background","editorPane.background"]).ok_or("That VS Code theme has no \"editor.background\" color, so there is nothing to build a palette from.")?;
    let appearance = match value["type"].as_str().map(str::to_lowercase).as_deref() {
        Some("light" | "hc-light") => Appearance::Light,
        Some("dark" | "hc-black") => Appearance::Dark,
        _ => {
            if luminance(canvas) < 0.179 {
                Appearance::Dark
            } else {
                Appearance::Light
            }
        }
    };
    let canvas_hex = hex(canvas);
    let raised = solid(
        colors,
        canvas,
        &["editorWidget.background", "dropdown.background"],
    );
    let mut accent = None;
    for key in [
        "focusBorder",
        "button.background",
        "textLink.foreground",
        "activityBarBadge.background",
        "progressBar.background",
        "badge.background",
    ] {
        if let Some(candidate) = parse(&colors[key]) {
            let color = flatten(candidate, canvas);
            if apart(&color, &canvas_hex)
                && raised.as_ref().is_none_or(|raised| apart(&color, raised))
            {
                accent = Some((candidate, color));
                break;
            }
        }
    }
    let (accent_color, accent_hex) = accent.unwrap_or_else(|| {
        let standard = color::hex(&catalog.data.standard[&appearance]["accent"]).unwrap();
        let chosen = [standard.as_str(), "#ffffff", "#000000"]
            .into_iter()
            .find(|v| {
                apart(v, &canvas_hex) && raised.as_ref().is_none_or(|raised| apart(v, raised))
            })
            .unwrap_or(&standard)
            .to_owned();
        (parse(&json!(chosen)).unwrap(), chosen)
    });
    let muted = flatten(
        Rgba {
            a: 0.2,
            ..accent_color
        },
        canvas,
    );
    let derived = vivid::create(catalog, appearance, &canvas_hex, &muted);
    let mut result = derived.clone();
    let sidebar = solid(
        colors,
        canvas,
        &["sideBar.background", "activityBar.background"],
    )
    .unwrap_or_else(|| derived["sidebar"].clone());
    let terminal = solid(colors, canvas, &["terminal.background", "panel.background"])
        .unwrap_or_else(|| derived["terminalBackground"].clone());
    let raised = raised.unwrap_or_else(|| derived["surfaceRaised"].clone());
    let action = solid(colors, canvas, &["button.background"])
        .filter(|v| apart(v, &canvas_hex) && apart(v, &raised))
        .unwrap_or_else(|| accent_hex.clone());
    let mut input = [
        derived["input"].as_str(),
        derived["surfaceRaised"].as_str(),
        catalog.data.standard[&appearance]["input"].as_str(),
        "#000000",
        "#ffffff",
        "#808080",
    ]
    .into_iter()
    .find(|v| apart(v, &canvas_hex) && apart(v, &action))
    .unwrap_or("#808080")
    .to_owned();
    for key in ["input.background", "input.border"] {
        if let Some(candidate) =
            solid(colors, canvas, &[key]).filter(|v| apart(v, &canvas_hex) && apart(v, &action))
        {
            input = candidate;
            break;
        }
    }
    macro_rules! overlay {
        ($role:literal,$base:expr,$keys:expr) => {
            result.insert(
                $role.into(),
                solid(colors, $base, $keys).unwrap_or_else(|| derived[$role].clone()),
            );
        };
    }
    macro_rules! text {
        ($role:literal,$surface:expr,$keys:expr) => {
            result.insert(
                $role.into(),
                readable(colors, $surface, &derived[$role], $keys),
            );
        };
    }
    result.insert("canvas".into(), canvas_hex.clone());
    text!("text", &canvas_hex, &["editor.foreground", "foreground"]);
    text!(
        "textMuted",
        &canvas_hex,
        &["descriptionForeground", "disabledForeground"]
    );
    overlay!("surface", canvas, &["editorWidget.background"]);
    result.insert("surfaceRaised".into(), raised.clone());
    overlay!(
        "surfaceOverlay",
        canvas,
        &[
            "menu.background",
            "quickInput.background",
            "dropdown.background"
        ]
    );
    overlay!(
        "border",
        canvas,
        &["panel.border", "editorGroup.border", "contrastBorder"]
    );
    result.insert("input".into(), input);
    text!("placeholder", &raised, &["input.placeholderForeground"]);
    text!(
        "error",
        &canvas_hex,
        &["editorError.foreground", "errorForeground"]
    );
    text!("warning", &canvas_hex, &["editorWarning.foreground"]);
    overlay!(
        "accentSurface",
        canvas,
        &["list.activeSelectionBackground", "list.hoverBackground"]
    );
    overlay!("codeBackground", canvas, &["textCodeBlock.background"]);
    result.insert("sidebar".into(), sidebar.clone());
    text!("sidebarForeground", &sidebar, &["sideBar.foreground"]);
    overlay!("sidebarBorder", rgb(&sidebar), &["sideBar.border"]);
    overlay!("sidebarRowHover", rgb(&sidebar), &["list.hoverBackground"]);
    overlay!(
        "sidebarRowActive",
        rgb(&sidebar),
        &["list.inactiveSelectionBackground", "list.hoverBackground"]
    );
    overlay!(
        "sidebarRowSelected",
        rgb(&sidebar),
        &["list.activeSelectionBackground"]
    );
    result.insert("terminalBackground".into(), terminal.clone());
    text!("terminalForeground", &terminal, &["terminal.foreground"]);
    overlay!(
        "terminalCursor",
        rgb(&terminal),
        &["terminalCursor.foreground", "editorCursor.foreground"]
    );
    overlay!(
        "terminalSelection",
        rgb(&terminal),
        &["terminal.selectionBackground", "editor.selectionBackground"]
    );
    overlay!(
        "terminalScrollbar",
        rgb(&terminal),
        &["scrollbarSlider.background"]
    );
    for role in ["accent", "focus"] {
        result.insert(role.into(), accent_hex.clone());
    }
    result.insert("messageAction".into(), action.clone());
    text!("messageActionForeground", &action, &["button.foreground"]);
    text!("accentForeground", &accent_hex, &["button.foreground"]);
    let name = [&value["displayName"], &value["name"]]
        .into_iter()
        .filter_map(Value::as_str)
        .map(humanize_name)
        .find(|s| !s.is_empty())
        .map(|s| slice_name(&s))
        .unwrap_or_else(|| "VS Code theme".into());
    let id = library::id_from_name(&name);
    let ordered: serde_json::Map<String, Value> = catalog
        .data
        .roles
        .iter()
        .map(|role| (role.clone(), json!(result[role])))
        .collect();
    let mut file = json!({"version":1,"name":name,"appearance":appearance,"colors":ordered});
    if catalog.data.reserved.contains(&id) {
        file["id"] = json!(format!("{id}-vscode"));
    }
    library::import(catalog, &file)
}
fn rename(catalog: &Catalog, theme: &Definition, name: &str) -> Option<Definition> {
    let mut file = json!({"version":1,"name":slice_name(name),"appearance":theme.appearance,"colors":theme.colors});
    if let Some(variants) = &theme.variants {
        file["variants"] = json!(variants);
    }
    if theme.managed == Some(true) {
        file["managed"] = json!(true);
    }
    library::import(catalog, &file).ok()
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub theme: Definition,
    pub source_name: Option<String>,
}
pub fn resolve_collisions(catalog: &Catalog, entries: &[Entry]) -> Vec<Definition> {
    static EXTENSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.[^.]+$").unwrap());
    let mut counts = BTreeMap::<&str, usize>::new();
    for entry in entries {
        *counts.entry(&entry.theme.id).or_default() += 1;
    }
    let renamed: Vec<_> = entries
        .iter()
        .map(|entry| {
            let theme = &entry.theme;
            if counts[theme.id.as_str()] < 2 {
                return theme.clone();
            }
            let stem = entry
                .source_name
                .as_deref()
                .map(|name| EXTENSION.replace(name, "").into_owned());
            let from_file = stem.as_deref().filter(|s| !s.is_empty()).map(humanize_name);
            from_file
                .filter(|name| name.to_lowercase() != theme.label.to_lowercase())
                .and_then(|name| rename(catalog, theme, &name))
                .unwrap_or_else(|| theme.clone())
        })
        .collect();
    let mut seen = BTreeSet::new();
    renamed
        .into_iter()
        .map(|theme| {
            if seen.insert(theme.id.clone()) {
                return theme;
            }
            for suffix in 2..100 {
                if let Some(candidate) =
                    rename(catalog, &theme, &format!("{} {suffix}", theme.label))
                {
                    if seen.insert(candidate.id.clone()) {
                        return candidate;
                    }
                }
            }
            theme
        })
        .collect()
}
struct Group {
    light: Vec<Definition>,
    dark: Vec<Definition>,
    order: usize,
}
pub fn pair(
    catalog: &Catalog,
    themes: &[Definition],
    paired_id: Option<&dyn Fn(&Definition, &Definition) -> String>,
) -> Vec<Definition> {
    static APPEARANCE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i-u:\b(?:light|dark)\b)").unwrap());
    let mut groups = Vec::<(String, Group)>::new();
    let mut passthrough = vec![];
    for (order, theme) in themes.iter().enumerate() {
        let stripped = APPEARANCE.replace_all(&theme.label, " ");
        let key = color::trim(
            &stripped
                .split(whitespace)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        )
        .to_owned();
        if theme.modes().len() != 1
            || key == theme.label
            || !key.bytes().any(|c| c.is_ascii_alphanumeric())
        {
            passthrough.push((order, theme.clone()));
            continue;
        }
        let index = groups
            .iter()
            .position(|(k, _)| k == &key)
            .unwrap_or_else(|| {
                groups.push((
                    key,
                    Group {
                        light: vec![],
                        dark: vec![],
                        order,
                    },
                ));
                groups.len() - 1
            });
        let group = &mut groups[index].1;
        match theme.appearance {
            Appearance::Light => group.light.push(theme.clone()),
            Appearance::Dark => group.dark.push(theme.clone()),
        };
    }
    for (name, group) in groups {
        if group.light.len() == 1 && group.dark.len() == 1 {
            let mut file = json!({"version":1,"name":name,"appearance":"light","colors":group.light[0].colors,"variants":{"dark":group.dark[0].colors}});
            if let Some(id) = paired_id {
                file["id"] = json!(id(&group.light[0], &group.dark[0]));
            }
            if let Ok(theme) = library::import(catalog, &file) {
                passthrough.push((group.order, theme));
                continue;
            }
        }
        for theme in group.light.into_iter().chain(group.dark) {
            passthrough.push((group.order, theme));
        }
    }
    passthrough.sort_by_key(|(order, _)| *order);
    passthrough.into_iter().map(|(_, theme)| theme).collect()
}
