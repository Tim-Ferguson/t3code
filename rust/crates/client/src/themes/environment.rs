//! Source useEnvironmentTheme.ts published palette normalization.
use super::{Appearance, Catalog, Colors, Definition, color, library, vivid};
use std::collections::BTreeMap;
use t3_contracts::{EnvironmentTheme, EnvironmentThemeAppearance, EnvironmentThemeColors};
fn overrides(catalog: &Catalog, raw: Option<&EnvironmentThemeColors>) -> Colors {
    raw.into_iter()
        .flat_map(|raw| raw.iter())
        .filter_map(|(role, value)| {
            catalog
                .data
                .roles
                .iter()
                .any(|r| r == role.as_str())
                .then(|| value.0.as_str())
                .and_then(color::canonical)
                .map(|color| (role.as_str().to_owned(), color))
        })
        .collect()
}
fn colors(
    catalog: &Catalog,
    theme: &EnvironmentTheme,
    mode: Appearance,
    raw: Option<&EnvironmentThemeColors>,
) -> Colors {
    let base = match (&theme.canvas, &theme.accent) {
        (Some(Some(canvas)), Some(Some(accent))) if mode == appearance(theme) => {
            vivid::create(catalog, mode, canvas.as_str(), accent.as_str())
        }
        _ => library::defaults(catalog, mode),
    };
    let mut result = base;
    result.extend(overrides(catalog, raw));
    result
}
fn appearance(theme: &EnvironmentTheme) -> Appearance {
    match theme.appearance {
        EnvironmentThemeAppearance::Light => Appearance::Light,
        EnvironmentThemeAppearance::Dark => Appearance::Dark,
    }
}
pub fn definition(catalog: &Catalog, theme: &EnvironmentTheme) -> Definition {
    let mode = appearance(theme);
    let mut variants = BTreeMap::new();
    if let Some(Some(raw)) = &theme.variants {
        for (variant, raw) in [
            (Appearance::Light, &raw.light),
            (Appearance::Dark, &raw.dark),
        ] {
            if variant != mode {
                if let Some(Some(raw)) = raw {
                    variants.insert(variant, colors(catalog, theme, variant, Some(raw)));
                }
            }
        }
    }
    Definition {
        id: theme.id.as_str().into(),
        label: theme.name.0.as_str().into(),
        appearance: mode,
        colors: colors(
            catalog,
            theme,
            mode,
            theme.colors.as_ref().and_then(Option::as_ref),
        ),
        variants: (!variants.is_empty()).then_some(variants),
        collection: None,
        sidebar_artwork: None,
        managed: (theme.canvas.as_ref().and_then(Option::as_ref).is_some()
            && theme.accent.as_ref().and_then(Option::as_ref).is_some()
            && theme.colors.as_ref().and_then(Option::as_ref).is_none()
            && theme.variants.as_ref().and_then(Option::as_ref).is_none())
        .then_some(true),
    }
}
pub fn definitions(catalog: &Catalog, themes: &[EnvironmentTheme]) -> Vec<Definition> {
    themes
        .iter()
        .filter(|theme| {
            if catalog
                .data
                .reserved
                .iter()
                .any(|id| id == theme.id.as_str())
            {
                return false;
            }
            if theme.canvas.as_ref().and_then(Option::as_ref).is_some()
                && theme.accent.as_ref().and_then(Option::as_ref).is_some()
            {
                return true;
            }
            let other = theme
                .variants
                .as_ref()
                .and_then(Option::as_ref)
                .and_then(|v| match appearance(theme) {
                    Appearance::Light => v.dark.as_ref(),
                    Appearance::Dark => v.light.as_ref(),
                })
                .and_then(Option::as_ref);
            !overrides(catalog, theme.colors.as_ref().and_then(Option::as_ref)).is_empty()
                || !overrides(catalog, other).is_empty()
        })
        .map(|theme| definition(catalog, theme))
        .collect()
}
