//! Searching and grouping the models in the picker, ported from `modelPickerSearch.ts` and
//! `ModelPickerContent.tsx` (replayed in `tests/composer_menu_parity.rs`).
use crate::composer_menu::{normalize_query, score_tiers};
use std::collections::BTreeSet;

/// What the search looks at in one model.
pub struct Searchable<'a> {
    pub driver: &'a str,
    pub provider_display_name: &'a str,
    pub name: &'a str,
    pub short_name: Option<&'a str>,
    pub sub_provider: Option<&'a str>,
    pub favorite: bool,
}

const FAVORITE_BOOST: f64 = 24.;

fn fields(model: &Searchable) -> Vec<String> {
    fn keep<'a>(value: &Option<&'a str>) -> Option<&'a str> {
        value.filter(|value| !value.is_empty())
    }
    let all: String = [Some(model.name), keep(&model.short_name), keep(&model.sub_provider), Some(model.driver), Some(model.provider_display_name)].into_iter().flatten().filter(|value| !value.is_empty()).collect::<Vec<_>>().join(" ");
    let mut out = vec![norm(model.name)];
    out.extend(keep(&model.short_name).map(norm));
    out.extend(keep(&model.sub_provider).map(norm));
    out.push(norm(model.driver));
    out.push(norm(model.provider_display_name));
    out.push(norm(&all));
    out
}
fn norm(text: &str) -> String {
    normalize_query(text, |_| false)
}

/// `scoreModelPickerSearch`: lower is better; `None` when a word of the query matches nothing.
pub fn score_search(model: &Searchable, query: &str) -> Option<f64> {
    let normalized = norm(query);
    let tokens: Vec<&str> = normalized.split_whitespace().collect();
    if tokens.is_empty() {
        return Some(0.);
    }
    let fields = fields(model);
    let mut total = 0.;
    for token in tokens {
        let best = fields.iter().enumerate().filter_map(|(index, field)| score_tiers(field, token, (index * 10) as f64, token.chars().count() >= 3)).min_by(|a, b| a.partial_cmp(b).unwrap())?;
        total += best;
    }
    Some(if model.favorite { total - FAVORITE_BOOST } else { total })
}

/// Which provider list the picker is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tab {
    Favorites,
    Instance(String),
}

/// One model as the picker lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub instance_id: String,
    pub driver: String,
    pub provider_label: String,
    pub slug: String,
    pub name: String,
    pub short_name: Option<String>,
    pub sub_provider: Option<String>,
}
impl Entry {
    pub fn key(&self) -> (String, String) {
        (self.instance_id.clone(), self.slug.clone())
    }
}

/// The rows to show, in order: a search looks across every provider and ranks; otherwise the
/// selected tab's models in catalogue order.
pub fn visible(entries: &[Entry], favorites: &BTreeSet<(String, String)>, query: &str, tab: &Tab) -> Vec<usize> {
    if !query.trim().is_empty() {
        let mut scored: Vec<(usize, f64)> = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let model = Searchable { driver: &entry.driver, provider_display_name: &entry.provider_label, name: &entry.name, short_name: entry.short_name.as_deref(), sub_provider: entry.sub_provider.as_deref(), favorite: favorites.contains(&entry.key()) };
                Some((index, score_search(&model, query)?))
            })
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap().then(a.0.cmp(&b.0)));
        return scored.into_iter().map(|(index, _)| index).collect();
    }
    (0..entries.len())
        .filter(|index| match tab {
            Tab::Favorites => favorites.contains(&entries[*index].key()),
            Tab::Instance(id) => &entries[*index].instance_id == id,
        })
        .collect()
}

/// The tab a picker opens on: favourites when there are any (and the selection is not elsewhere),
/// otherwise the selected model's provider.
pub fn initial_tab(favorites: &BTreeSet<(String, String)>, selected_instance: &str) -> Tab {
    if favorites.is_empty() { Tab::Instance(selected_instance.to_owned()) } else { Tab::Favorites }
}
