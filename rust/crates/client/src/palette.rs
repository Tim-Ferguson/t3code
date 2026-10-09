//! The command palette's items and how a query narrows and orders them. Port of
//! `CommandPalette.logic.ts` (grouping, ranking, filtering, rows) and `normalizeSearchText`.
use icu_normalizer::DecomposingNormalizer;

/// `normalizeSearchText`: accents and case folded away, whitespace collapsed.
pub fn normalize_search_text(value: &str) -> String {
    let decomposed = DecomposingNormalizer::new_nfkd().normalize(value);
    let stripped: String = decomposed.chars().filter(|c| !is_mark(*c)).collect();
    stripped.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}
/// The combining-mark blocks `\p{M}` covers in text people search.
fn is_mark(c: char) -> bool {
    matches!(c, '\u{300}'..='\u{36f}' | '\u{483}'..='\u{489}' | '\u{591}'..='\u{5bd}' | '\u{610}'..='\u{61a}' | '\u{64b}'..='\u{65f}' | '\u{900}'..='\u{903}' | '\u{93a}'..='\u{94f}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}' | '\u{20d0}'..='\u{20ff}' | '\u{3099}'..='\u{309a}' | '\u{fe20}'..='\u{fe2f}')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Action,
    /// Opens a second page of items.
    Submenu,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentMatch {
    pub user: bool,
    pub snippet: String,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub kind: Kind,
    pub value: String,
    pub title: String,
    pub description: Option<String>,
    pub search_terms: Vec<String>,
    /// The keybinding command whose shortcut is shown beside the item.
    pub shortcut_command: Option<String>,
    pub disabled: bool,
    /// Sorts after every other match in its group.
    pub secondary: bool,
    /// A thread's last activity; its presence makes the title tie-break by recency.
    pub recency: Option<i64>,
    pub timestamp: Option<String>,
    pub content_match: Option<ContentMatch>,
    /// Runs without closing the palette.
    pub keep_open: bool,
    /// The page a submenu opens.
    pub groups: Vec<Group>,
}
impl Item {
    pub fn action(value: &str, title: &str, terms: &[&str]) -> Self {
        Self { kind: Kind::Action, value: value.into(), title: title.into(), description: None, search_terms: terms.iter().map(|term| (*term).to_owned()).collect(), shortcut_command: None, disabled: false, secondary: false, recency: None, timestamp: None, content_match: None, keep_open: false, groups: vec![] }
    }
    pub fn submenu(value: &str, title: &str, terms: &[&str], groups: Vec<Group>) -> Self {
        Self { kind: Kind::Submenu, groups, ..Self::action(value, title, terms) }
    }
    pub fn shortcut(mut self, command: &str) -> Self {
        self.shortcut_command = Some(command.into());
        self
    }
    pub fn keeping_open(mut self) -> Self {
        self.keep_open = true;
        self
    }
    pub fn described(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub value: String,
    pub label: String,
    pub items: Vec<Item>,
}
impl Group {
    pub fn new(value: &str, label: &str, items: Vec<Item>) -> Self {
        Self { value: value.into(), label: label.into(), items }
    }
}

/// `buildRootGroups`.
pub fn root_groups(actions: Vec<Item>, recent_threads: Vec<Item>) -> Vec<Group> {
    let mut groups = vec![];
    if !actions.is_empty() {
        groups.push(Group::new("actions", "Actions", actions));
    }
    if !recent_threads.is_empty() {
        groups.push(Group::new("recent-threads", "Recent Threads", recent_threads));
    }
    groups
}

fn rank_field(field: &str, query: &str, tokens: &[&str]) -> Option<i64> {
    let field = normalize_search_text(field);
    if field.is_empty() || !tokens.iter().all(|token| field.contains(token)) {
        return None;
    }
    Some(if field == query {
        3
    } else if field.starts_with(query) {
        2
    } else if field.contains(query) {
        1
    } else {
        0
    })
}
fn rank_item(item: &Item, query: &str, tokens: &[&str]) -> i64 {
    let terms: Vec<&String> = item.search_terms.iter().filter(|term| !term.is_empty()).collect();
    for (index, field) in terms.iter().enumerate() {
        if let Some(rank) = rank_field(field, query, tokens) {
            if index == 0 && item.recency.is_some() {
                // All non-exact thread title matches share a tier so recency breaks the tie.
                return 1000 + i64::from(rank == 3);
            }
            return 1000 - index as i64 * 100 + rank;
        }
    }
    0
}

/// What the palette searches besides the page it is on.
#[derive(Debug, Clone, Default)]
pub struct Extra {
    pub projects: Vec<Item>,
    pub settings: Vec<Item>,
    pub threads: Vec<Item>,
}

/// `filterCommandPaletteGroups`. A leading `>` limits the search to actions; on the root page a
/// query also searches projects, settings and threads, and hides the recent-threads shortcut list.
pub fn filter_groups(active: &[Group], query: &str, in_submenu: bool, extra: &Extra) -> Vec<Group> {
    let actions_only = query.starts_with('>');
    let search = if actions_only { &query[1..] } else { query };
    let normalized = normalize_search_text(search);
    if normalized.is_empty() {
        return if actions_only { active.iter().filter(|group| group.value == "actions").cloned().collect() } else { active.to_vec() };
    }
    let tokens: Vec<&str> = normalized.split(' ').collect();
    let mut groups: Vec<Group> = active.to_vec();
    if actions_only {
        groups.retain(|group| group.value == "actions");
    } else if !in_submenu {
        groups.retain(|group| group.value != "recent-threads");
    }
    if !in_submenu && !actions_only {
        for (value, label, items) in [("projects-search", "Projects", &extra.projects), ("settings-search", "Settings", &extra.settings), ("threads-search", "Threads", &extra.threads)] {
            if !items.is_empty() {
                groups.push(Group::new(value, label, items.clone()));
            }
        }
    }
    groups
        .into_iter()
        .filter_map(|group| {
            let mut matched: Vec<(usize, i64, Item)> = group
                .items
                .into_iter()
                .enumerate()
                .filter(|(_, item)| {
                    let haystack = normalize_search_text(&item.search_terms.join(" "));
                    tokens.iter().all(|token| haystack.contains(token))
                })
                .map(|(index, item)| {
                    let rank = rank_item(&item, &normalized, &tokens);
                    (index, rank, item)
                })
                .collect();
            matched.sort_by(|left, right| {
                left.2.secondary.cmp(&right.2.secondary).then(right.1.cmp(&left.1)).then(right.2.recency.unwrap_or(0).cmp(&left.2.recency.unwrap_or(0))).then(left.0.cmp(&right.0))
            });
            (!matched.is_empty()).then(|| Group { value: group.value, label: group.label, items: matched.into_iter().map(|(_, _, item)| item).collect() })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    Label { key: String, label: String, first: bool },
    /// `index` is the position among enabled items, which the keyboard moves through.
    Item { key: String, item: Item, index: Option<usize> },
}
/// `buildCommandPaletteRows`: the flat rows, and the enabled items' values in keyboard order.
pub fn build_rows(groups: &[Group]) -> (Vec<Row>, Vec<String>) {
    let (mut rows, mut values) = (vec![], vec![]);
    for group in groups {
        if !group.label.is_empty() {
            rows.push(Row::Label { key: format!("group:{}", group.value), label: group.label.clone(), first: rows.is_empty() });
        }
        for item in &group.items {
            let index = (!item.disabled).then(|| {
                values.push(item.value.clone());
                values.len() - 1
            });
            rows.push(Row::Item { key: format!("{}:{}", group.value, item.value), item: item.clone(), index });
        }
    }
    (rows, values)
}
/// `findHighlightedCommandPaletteItem`.
pub fn find_item<'a>(groups: &'a [Group], value: &str) -> Option<&'a Item> {
    groups.iter().flat_map(|group| &group.items).find(|item| item.value == value && !item.disabled)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Root,
    Submenu,
}
/// `getCommandPaletteInputPlaceholder`.
pub fn placeholder(page: Page) -> &'static str {
    match page {
        Page::Root => "Search commands, projects, and threads...",
        Page::Submenu => "Search...",
    }
}
/// The empty state's wording.
pub fn empty_message(actions_only: bool) -> &'static str {
    if actions_only { "No matching actions." } else { "No matching commands, projects, or threads." }
}

/// `HighlightedSearchText`: the text cut into runs, the ones equal to the query (ignoring ASCII
/// case) marked.
pub fn highlight(text: &str, query: &str) -> Vec<(String, bool)> {
    let query = query.trim();
    if query.is_empty() {
        return vec![(text.to_owned(), false)];
    }
    let (folded_text, folded_query) = (text.to_ascii_lowercase(), query.to_ascii_lowercase());
    let (mut parts, mut cursor) = (vec![], 0);
    while cursor < text.len() {
        match folded_text[cursor..].find(&folded_query) {
            None => {
                parts.push((text[cursor..].to_owned(), false));
                break;
            }
            Some(offset) => {
                let start = cursor + offset;
                if start > cursor {
                    parts.push((text[cursor..start].to_owned(), false));
                }
                parts.push((text[start..start + folded_query.len()].to_owned(), true));
                cursor = start + folded_query.len();
            }
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str, title: &str, recency: i64) -> Item {
        let mut item = Item::action(&format!("thread:{id}"), title, &[title, "Project", id]);
        item.recency = Some(recency);
        item
    }
    fn titles(groups: &[Group]) -> Vec<Vec<String>> {
        groups.iter().map(|group| group.items.iter().map(|item| item.title.clone()).collect()).collect()
    }
    fn actions() -> Vec<Group> {
        vec![Group::new("actions", "Actions", vec![
            Item::action("a:new", "New thread", &["new thread", "chat", "create"]).shortcut("chat.new"),
            Item::action("a:diff", "Toggle diff panel", &["diff", "changes"]).shortcut("diff.toggle"),
            Item::action("a:settings", "Open settings", &["settings", "preferences"]),
            Item::action("a:theme", "Change theme", &["change theme", "appearance"]),
        ])]
    }

    #[test]
    fn normalization_folds_case_accents_width_and_spacing() {
        assert_eq!(normalize_search_text("  Café   NOÏR "), "cafe noir");
        assert_eq!(normalize_search_text("ＦＵＬＬ"), "full");
        assert_eq!(normalize_search_text("ﬁle"), "file");
    }

    #[test]
    fn every_token_must_match_and_better_matches_come_first() {
        let groups = actions();
        assert_eq!(titles(&filter_groups(&groups, "theme", false, &Extra::default())), [["Change theme"]]);
        // The title-equal / prefix / contains tiers order the matches, earlier search terms first.
        let more = vec![Group::new("actions", "Actions", vec![Item::action("1", "Add the thing", &["add the thing"]), Item::action("2", "Thing", &["thing"]), Item::action("3", "Things to do", &["things to do"])])];
        assert_eq!(titles(&filter_groups(&more, "thing", false, &Extra::default())), [["Thing", "Things to do", "Add the thing"]]);
        assert!(filter_groups(&groups, "diff zzz", false, &Extra::default()).is_empty());
        assert_eq!(filter_groups(&groups, "   ", false, &Extra::default()), groups);
    }

    #[test]
    fn a_query_adds_projects_settings_and_threads_and_drops_the_recent_list() {
        let mut groups = actions();
        groups.push(Group::new("recent-threads", "Recent Threads", vec![thread("t1", "Fix login", 5)]));
        let extra = Extra { projects: vec![Item::action("p:1", "Login service", &["login service", "/work/login"])], settings: vec![], threads: vec![thread("t1", "Fix login", 5), thread("t2", "Login page", 9)] };
        let shown = filter_groups(&groups, "login", false, &extra);
        assert_eq!(shown.iter().map(|group| group.label.as_str()).collect::<Vec<_>>(), ["Projects", "Threads"]);
        // Threads tie on tier, so the more recent one leads.
        assert_eq!(titles(&shown)[1], ["Login page", "Fix login"]);
        // Inside a submenu only that page is searched, and the recent-thread list stays.
        let page = filter_groups(&groups, "login", true, &extra);
        assert_eq!(page.iter().map(|group| group.label.as_str()).collect::<Vec<_>>(), ["Recent Threads"]);
    }

    #[test]
    fn a_leading_angle_bracket_limits_the_search_to_actions() {
        let mut groups = actions();
        groups.push(Group::new("recent-threads", "Recent Threads", vec![thread("t1", "Settings thread", 5)]));
        let extra = Extra { threads: vec![thread("t1", "Settings thread", 5)], ..Default::default() };
        assert_eq!(titles(&filter_groups(&groups, ">settings", false, &extra)), [["Open settings"]]);
        assert_eq!(filter_groups(&groups, ">", false, &extra).len(), 1);
        assert_eq!(filter_groups(&groups, ">", false, &extra)[0].value, "actions");
    }

    #[test]
    fn secondary_matches_sort_last_and_rows_skip_disabled_items_for_the_keyboard() {
        let mut late = Item::action("late", "Late", &["alpha late"]);
        late.secondary = true;
        let mut off = Item::action("off", "Off", &["alpha off"]);
        off.disabled = true;
        let groups = vec![Group::new("g", "Group", vec![late, off, Item::action("early", "Early", &["alpha"])])];
        let shown = filter_groups(&groups, "alpha", false, &Extra::default());
        assert_eq!(titles(&shown), [["Early", "Off", "Late"]]);
        let (rows, values) = build_rows(&shown);
        assert_eq!(values, ["early", "late"]);
        assert!(matches!(&rows[0], Row::Label { first: true, .. }));
        assert!(matches!(&rows[2], Row::Item { index: None, .. }));
        assert_eq!(find_item(&shown, "off"), None);
        assert!(find_item(&shown, "late").is_some());
    }

    #[test]
    fn highlighting_marks_each_ascii_case_insensitive_occurrence() {
        assert_eq!(highlight("Fix the Login, then login", "login"), vec![("Fix the ".to_owned(), false), ("Login".to_owned(), true), (", then ".to_owned(), false), ("login".to_owned(), true)]);
        assert_eq!(highlight("nothing here", "zzz"), vec![("nothing here".to_owned(), false)]);
        assert_eq!(highlight("abc", "  "), vec![("abc".to_owned(), false)]);
        // Multi-byte text around a match keeps its boundaries.
        assert_eq!(highlight("héllo wörld", "wör").iter().map(|(text, on)| (text.as_str(), *on)).collect::<Vec<_>>(), [("héllo ", false), ("wör", true), ("ld", false)]);
    }
}
