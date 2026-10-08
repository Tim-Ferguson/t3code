//! Source appearanceFonts.ts and Ghostty surface font policy.
pub const GLYPH_FALLBACKS: &str = "\"Symbols Nerd Font Mono\", \"Symbols Nerd Font\", \"JetBrainsMono Nerd Font\", \"JetBrainsMono NF\", \"FiraCode Nerd Font\", \"Hack Nerd Font\", \"MesloLGS NF\", \"CaskaydiaCove Nerd Font\", \"PowerlineSymbols\", monospace";
pub const DEFAULT_FONT: &str = "\"SF Mono\", \"SFMono-Regular\", Menlo, Consolas, \"Liberation Mono\", \"Symbols Nerd Font Mono\", \"Symbols Nerd Font\", \"JetBrainsMono Nerd Font\", \"JetBrainsMono NF\", \"FiraCode Nerd Font\", \"Hack Nerd Font\", \"MesloLGS NF\", \"CaskaydiaCove Nerd Font\", \"PowerlineSymbols\", monospace";
fn trim(s: &str) -> &str {
    s.trim_matches(|ch|matches!(ch,'\u{0009}'..='\u{000d}'|'\u{0020}'|'\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}'))
}
pub fn quote_families(input: &str) -> String {
    input
        .split(',')
        .filter_map(|name| {
            let bare = trim(name);
            if bare.is_empty() {
                return None;
            }
            let quoted = (bare.starts_with('"') && bare.ends_with('"')
                || bare.starts_with('\'') && bare.ends_with('\''))
                && bare.len() >= 2
                && !bare.contains(['\n', '\r', '\u{2028}', '\u{2029}']);
            let ident = bare.as_bytes()[0].is_ascii_alphabetic()
                && bare.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            Some(if quoted || ident {
                bare.to_owned()
            } else {
                format!("\"{}\"", bare.replace('"', ""))
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}
pub fn unchecked_family(input: &str) -> String {
    let family = quote_families(input);
    if family.is_empty() {
        DEFAULT_FONT.into()
    } else {
        format!("{family}, {GLYPH_FALLBACKS}")
    }
}
pub fn size(value: f64) -> f64 {
    if value.is_finite() {
        (value + 0.5).floor().clamp(6.0, 32.0)
    } else {
        12.0
    }
}
pub fn monospace_advances(advances: &[f64]) -> bool {
    let Some(reference) = advances.first() else {
        return true;
    };
    if *reference <= 0.0
        || advances
            .iter()
            .any(|width| !width.is_finite() || *width <= 0.0)
    {
        return true;
    }
    advances
        .iter()
        .all(|width| (*width - reference).abs() < 0.01)
}
