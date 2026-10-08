//! ThemeSettings collection-card ordering, labels and mode defaults.
use super::{Appearance, Definition, color};
use std::collections::BTreeMap;
pub fn groups(themes: &[Definition]) -> Vec<(String, Vec<Definition>)> {
    let mut groups = Vec::<(String, Vec<Definition>)>::new();
    let mut positions = BTreeMap::new();
    for theme in themes {
        let key = theme
            .collection
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(|id| format!("collection:{id}"))
            .unwrap_or_else(|| format!("theme:{}", theme.id));
        let index = *positions.entry(key.clone()).or_insert_with(|| {
            groups.push((key, Vec::new()));
            groups.len() - 1
        });
        groups[index].1.push(theme.clone());
    }
    groups
}
/// JavaScript's locale is supplied by the owning WebView, not inferred from the host OS.
pub fn variant_labels(labels: &[String], lowercase: impl Fn(&str) -> String) -> Vec<String> {
    let words: Vec<Vec<&str>> = labels
        .iter()
        .map(|label| {
            let trimmed = color::trim(label);
            if trimmed.is_empty() {
                return vec![""];
            }
            trimmed
                .split(color::whitespace)
                .filter(|word| !word.is_empty())
                .collect()
        })
        .collect();
    let Some(first) = words.first() else {
        return vec![];
    };
    let folded: Vec<Vec<String>> = words
        .iter()
        .map(|words| words.iter().map(|word| lowercase(word)).collect())
        .collect();
    let prefix = first
        .iter()
        .enumerate()
        .find(|(index, _)| {
            folded
                .iter()
                .any(|words| words.get(*index) != folded[0].get(*index))
        })
        .map(|(index, _)| index)
        .unwrap_or_else(|| first.len().saturating_sub(1));
    words
        .iter()
        .zip(labels)
        .map(|(words, label)| {
            let short = words
                .iter()
                .skip(prefix)
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            let short = color::trim(&short);
            if short.is_empty() {
                label.clone()
            } else {
                short.into()
            }
        })
        .collect()
}
pub fn initial_index(themes: &[Definition], active: impl Fn(&str) -> bool) -> usize {
    themes.iter().position(|t| active(&t.id)).unwrap_or(0)
}
pub fn safe_index(index: usize, length: usize) -> Option<usize> {
    length.checked_sub(1).map(|last| index.min(last))
}
pub fn defaults(themes: &[Definition]) -> Vec<(Appearance, String)> {
    [Appearance::Light, Appearance::Dark]
        .into_iter()
        .filter_map(|mode| {
            themes
                .iter()
                .find(|t| t.colors(mode).is_some())
                .map(|t| (mode, t.id.clone()))
        })
        .collect()
}
