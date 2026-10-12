//! What the Keybindings page lists and checks: one row per rule, grouped as the original groups
//! them, with the conflicts a shortcut would create and the default a custom rule can be reset
//! to. Port of `KeybindingsSettings.logic.ts`, restricted to the commands this app can carry out.
use crate::keybindings::{CONTEXT_NAMES, Rule, parse_when, shortcut_to_input};
use std::collections::{BTreeMap, BTreeSet};

/// The commands of the original's set whose features this app has (the ones `keys.rs` carries out). Every other
/// command stays in the file untouched, has no palette entry, and its shortcut does nothing.
pub const WIRED_COMMANDS: &[&str] = &[
    "sidebar.toggle",
    "commandPalette.toggle",
    "chat.new",
    "chat.newWithoutProject",
    "diff.toggle",
    "terminal.toggle",
    "theme.select",
    "appearance.cycle",
    "composer.branch",
    "thread.previous",
    "thread.next",
    "thread.stop",
    "thread.jump.1",
    "thread.jump.2",
    "thread.jump.3",
    "thread.jump.4",
    "thread.jump.5",
    "thread.jump.6",
    "thread.jump.7",
    "thread.jump.8",
    "thread.jump.9",
    // What `keys.rs` carries out beyond the first set: the terminal, the composer, the queue, usage, the theme editor,
    // the commands a thread row's menu has, the tabbed panel and the page's history. The original's preview browser, thread
    // details and host commands have no feature here, so they stay out.
    "navigation.back",
    "navigation.forward",
    "rightPanel.toggle",
    "rightPanel.new",
    "rightPanel.toggleMaximized",
    "rightPanel.close",
    "view.reopenClosed",
    "terminal.split",
    "terminal.splitVertical",
    "terminal.new",
    "terminal.close",
    "pullRequest.copyNumber",
    "usage.open",
    "usage.cost",
    "usage.tokens",
    "usage.limits",
    "usage.period.day",
    "usage.period.week",
    "usage.period.month",
    "usage.period.quarter",
    "themeEditor.toggle",
    "composer.stash",
    "composer.sendAlternate",
    "composer.sendBackground",
    "composer.sendAndNewThread",
    "composer.effort",
    "composer.mode",
    "composer.workspace",
    "composer.host",
    "composer.cycleHost",
    "composer.previousWorktree",
    "chat.newLocal",
    "editor.openFavorite",
    "modelPicker.toggle",
    "modelPicker.previousProvider",
    "modelPicker.nextProvider",
    // The file picker and the project search open the command palette in their mode (`keys.rs`); the picker's own number keys
    // choose the nth model while it is open.
    "filePicker.toggle",
    "projectSearch.toggle",
    "modelPicker.jump.1",
    "modelPicker.jump.2",
    "modelPicker.jump.3",
    "modelPicker.jump.4",
    "modelPicker.jump.5",
    "modelPicker.jump.6",
    "modelPicker.jump.7",
    "modelPicker.jump.8",
    "modelPicker.jump.9",
    "thread.steerQueuedMessage",
    "thread.editQueuedMessage",
    "thread.copyReference",
    "thread.settle",
    "thread.pin",
    "thread.undo",
];
pub fn is_wired(command: &str) -> bool {
    WIRED_COMMANDS.contains(&command)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    Custom,
}
impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub command: String,
    pub key: String,
    pub when: String,
    pub source: Source,
    pub default_key: Option<String>,
    pub default_when: String,
    pub conflicts: Vec<String>,
}

/// `commandLabel`.
pub fn command_label(command: &str) -> String {
    match command {
        "composer.sendAlternate" => return "Composer: Opposite Queue or Steer Action".into(),
        "composer.sendBackground" => return "Composer: Start in Background".into(),
        "composer.sendAndNewThread" => return "Composer: Send and Start New Thread".into(),
        "thread.steerQueuedMessage" => return "Queue: Send First Queued Message as Steer".into(),
        "thread.editQueuedMessage" => return "Queue: Edit Last Queued Message".into(),
        "thread.copyReference" => return "Pull Request: Copy Link or Thread ID".into(),
        "view.reopenClosed" => return "Reopen Closed Tab".into(),
        _ => {}
    }
    command.split('.').map(title_case_segment).collect::<Vec<_>>().join(": ")
}
/// Splits camelCase and separators into capitalised words.
fn title_case_segment(segment: &str) -> String {
    let mut spaced = String::new();
    let mut previous: Option<char> = None;
    for c in segment.chars() {
        if c.is_ascii_uppercase() && previous.is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit()) {
            spaced.push(' ');
        }
        spaced.push(c);
        previous = Some(c);
    }
    spaced
        .split(|c: char| c == '-' || c == '_' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn spelled(rule: &Rule) -> (String, String) {
    (shortcut_to_input(&rule.shortcut), rule.when.as_ref().map(|when| when.expression()).unwrap_or_default())
}

/// The spelling (key, condition) of the last rule for a command, which is the one in effect: what a change to it names as the old one.
pub fn spelling_of(rules: &[Rule], command: &str) -> Option<(String, String)> {
    rules.iter().rev().find(|rule| rule.command == command).map(spelled)
}

fn defaults() -> &'static [Rule] {
    static RULES: std::sync::OnceLock<Vec<Rule>> = std::sync::OnceLock::new();
    RULES.get_or_init(crate::keybindings::default_rules)
}
fn default_for(rule: &Rule) -> Option<&'static Rule> {
    let (key, when) = spelled(rule);
    let defaults = defaults();
    defaults
        .iter()
        .find(|entry| entry.command == rule.command && spelled(entry) == (key.clone(), when.clone()))
        .or_else(|| defaults.iter().find(|entry| entry.command == rule.command && spelled(entry).1 == when))
        .or_else(|| defaults.iter().find(|entry| entry.command == rule.command))
}

/// `conflictsWithWhen`: an empty condition means always, which overlaps every other.
fn overlaps(left: &str, right: &str) -> bool {
    left.is_empty() || right.is_empty() || left == right
}
/// `keybindingConflictLabels`: the other commands a key under this condition would collide with.
pub fn conflict_labels(rows: &[Row], row_id: &str, key: &str, when: &str) -> Vec<String> {
    if key.trim().is_empty() {
        return vec![];
    }
    let labels: BTreeSet<String> = rows.iter().filter(|row| row.id != row_id && row.key == key && overlaps(&row.when, when)).map(|row| command_label(&row.command)).collect();
    labels.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: &'static str,
    pub title: &'static str,
    pub rows: Vec<Row>,
}
const GROUPS: &[(&str, &str, &[&str])] = &[
    ("navigation", "Navigation", &["sidebar", "rightPanel", "commandPalette", "filePicker", "projectSearch", "editor"]),
    ("threads", "Threads", &["thread", "chat", "pullRequest"]),
    ("composer", "Composer", &["composer", "modelPicker"]),
    ("terminal", "Terminal", &["terminal"]),
    ("preview", "Preview & diff", &["preview", "diff"]),
    ("appearance", "Appearance", &["theme", "appearance", "themeEditor"]),
];
pub fn group_rows(rows: Vec<Row>) -> Vec<Group> {
    let mut groups: Vec<Group> = GROUPS.iter().map(|(id, title, _)| Group { id, title, rows: vec![] }).collect();
    groups.push(Group { id: "other", title: "Other", rows: vec![] });
    for row in rows {
        let prefix = row.command.split('.').next().unwrap_or("");
        let index = GROUPS.iter().position(|(_, _, prefixes)| prefixes.contains(&prefix)).unwrap_or(GROUPS.len());
        groups[index].rows.push(row);
    }
    groups.retain(|group| !group.rows.is_empty());
    groups
}

/// `buildKeybindingRows`, for the commands this app has, filtered by the search text.
pub fn build_rows(rules: &[Rule], query: &str) -> Vec<Row> {
    let query = query.trim().to_lowercase();
    let mut rows: Vec<Row> = rules
        .iter()
        .enumerate()
        .filter(|(_, rule)| is_wired(&rule.command))
        .map(|(index, rule)| {
            let (key, when) = spelled(rule);
            let default = default_for(rule);
            let is_default = defaults().iter().any(|entry| entry.command == rule.command && spelled(entry) == (key.clone(), when.clone()));
            Row {
                id: format!("{}\0{key}\0{when}\0{index}", rule.command),
                command: rule.command.clone(),
                source: if is_default { Source::Default } else { Source::Custom },
                default_key: default.map(|entry| spelled(entry).0),
                default_when: default.map(|entry| spelled(entry).1).unwrap_or_default(),
                key,
                when,
                conflicts: vec![],
            }
        })
        .collect();
    let snapshot = rows.clone();
    for row in &mut rows {
        row.conflicts = conflict_labels(&snapshot, &row.id, &row.key, &row.when);
    }
    rows.sort_by(|left, right| command_order(&left.command).cmp(&command_order(&right.command)).then_with(|| left.key.cmp(&right.key)));
    if query.is_empty() {
        return rows;
    }
    rows.retain(|row| [row.command.to_lowercase(), command_label(&row.command).to_lowercase(), row.key.to_lowercase(), row.when.to_lowercase(), row.source.label().to_lowercase()].iter().any(|field| field.contains(&query)));
    rows
}
/// The jump commands sort by number, everything else by label.
fn command_order(command: &str) -> (String, String) {
    (command_label(command), command.to_owned())
}

/// How many resolved rules belong to commands this app has no feature for.
pub fn hidden_count(rules: &[Rule]) -> usize {
    rules.iter().filter(|rule| !is_wired(&rule.command)).count()
}

/// The commands a new rule can be given.
pub fn command_options() -> Vec<(&'static str, String)> {
    let mut options: Vec<_> = WIRED_COMMANDS.iter().map(|command| (*command, command_label(command))).collect();
    options.sort_by(|left, right| left.1.cmp(&right.1));
    options
}

/// `unknownWhenVariables`: names in an expression that no context defines.
pub fn unknown_when_variables(expression: &str) -> Vec<String> {
    let Some(ast) = parse_when(expression) else { return vec![] };
    let mut names = BTreeSet::new();
    ast.identifiers(&mut names);
    names.into_iter().filter(|name| !CONTEXT_NAMES.contains(&name.as_str()) && !defaults_use(name)).collect()
}
fn defaults_use(name: &str) -> bool {
    static NAMES: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names = BTreeSet::new();
        for rule in defaults() {
            if let Some(when) = &rule.when {
                when.identifiers(&mut names);
            }
        }
        names
    })
    .contains(name)
}

/// The text of a rule as it was written to the file. The server returns resolved rules only, and
/// removing or replacing one is matched against the text in the file, so the page keeps what it
/// wrote and falls back to the canonical spelling for rules it did not write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Written {
    texts: BTreeMap<(String, String, String), (String, String)>,
}
impl Written {
    /// Remembers that `key`/`when` was saved for `command`.
    pub fn remember(&mut self, command: &str, key: &str, when: &str) {
        if let Some(shortcut) = crate::keybindings::parse_shortcut(key) {
            let canonical_when = if when.is_empty() { String::new() } else { parse_when(when).map(|ast| ast.expression()).unwrap_or_default() };
            self.texts.insert((command.to_owned(), shortcut_to_input(&shortcut), canonical_when), (key.to_owned(), when.to_owned()));
        }
    }
    pub fn forget(&mut self, command: &str, key: &str, when: &str) {
        self.texts.remove(&(command.to_owned(), key.to_owned(), when.to_owned()));
    }
    /// The (key, when) text to send to name this row's rule in the file.
    pub fn text_for(&self, row: &Row) -> (String, String) {
        self.texts.get(&(row.command.clone(), row.key.clone(), row.when.clone())).cloned().unwrap_or_else(|| (row.key.clone(), row.when.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::merge_with_defaults;

    #[test]
    fn labels_split_camel_case_segments() {
        assert_eq!(command_label("commandPalette.toggle"), "Command Palette: Toggle");
        assert_eq!(command_label("chat.newWithoutProject"), "Chat: New Without Project");
        assert_eq!(command_label("thread.jump.3"), "Thread: Jump: 3");
        assert_eq!(command_label("view.reopenClosed"), "Reopen Closed Tab");
    }

    #[test]
    fn only_the_commands_this_app_has_are_listed_and_the_rest_are_counted() {
        let rules = merge_with_defaults(vec![]);
        let rows = build_rows(&rules, "");
        assert!(rows.iter().all(|row| is_wired(&row.command)));
        assert!(rows.iter().any(|row| row.command == "diff.toggle" && row.key == "mod+d" && row.when == "!terminalFocus" && row.source == Source::Default));
        assert!(!rows.iter().any(|row| row.command == "preview.toggle"));
        assert_eq!(rows.len() + hidden_count(&rules), rules.len());
        let groups = group_rows(rows);
        let titles: Vec<_> = groups.iter().map(|group| group.title).collect();
        // The original's usage, history and reopen commands belong to no named group, so they are listed under Other, as there.
        assert_eq!(titles, ["Navigation", "Threads", "Composer", "Terminal", "Preview & diff", "Appearance", "Other"]);
        assert!(groups.last().unwrap().rows.iter().all(|row| ["usage.", "navigation.", "view."].iter().any(|prefix| row.command.starts_with(prefix))));
        assert!(groups.last().unwrap().rows.iter().any(|row| row.command == "view.reopenClosed") && groups[0].rows.iter().any(|row| row.command == "rightPanel.toggle"));
    }

    #[test]
    fn every_shortcut_the_key_runner_carries_out_can_be_seen_and_rebound() {
        let rows = build_rows(&merge_with_defaults(vec![]), "");
        for command in ["filePicker.toggle", "projectSearch.toggle", "modelPicker.jump.1", "modelPicker.jump.9"] {
            assert!(rows.iter().any(|row| row.command == command && row.source == Source::Default), "{command} has a default shortcut and a row");
            assert!(command_options().iter().any(|(id, _)| *id == command), "{command} can be given to a new rule");
        }
    }

    #[test]
    fn a_custom_rule_is_marked_resettable_and_a_clash_names_the_other_command() {
        let mut rules = merge_with_defaults(vec![]);
        // chat.new moved onto the chord diff.toggle already has, under the same condition.
        rules.retain(|rule| rule.command != "chat.new");
        rules.push(Rule { command: "chat.new".into(), shortcut: crate::keybindings::parse_shortcut("mod+d").unwrap(), when: parse_when("!terminalFocus") });
        let rows = build_rows(&rules, "");
        let moved = rows.iter().find(|row| row.command == "chat.new").unwrap();
        assert_eq!(moved.source, Source::Custom);
        assert_eq!(moved.default_key.as_deref(), Some("mod+n"));
        assert_eq!(moved.conflicts, ["Diff: Toggle"]);
        // Different conditions that cannot both hold do not clash: terminal-only against not-terminal.
        let split = Row { id: "x".into(), command: "terminal.split".into(), key: "mod+d".into(), when: "terminalFocus".into(), source: Source::Default, default_key: None, default_when: String::new(), conflicts: vec![] };
        assert_eq!(conflict_labels(&[split.clone()], "other", "mod+d", "terminalFocus"), ["Terminal: Split"]);
        assert!(conflict_labels(&[split.clone()], "other", "mod+d", "!terminalFocus").is_empty());
        assert_eq!(conflict_labels(&[split], "other", "mod+d", ""), ["Terminal: Split"], "an empty condition overlaps everything");
        assert!(conflict_labels(&rows, "none", "  ", "").is_empty());
    }

    #[test]
    fn search_matches_label_key_condition_and_source() {
        let rules = merge_with_defaults(vec![]);
        for (query, command) in [("palette", "commandPalette.toggle"), ("mod+k", "commandPalette.toggle"), ("isdesktop", "thread.jump.1"), ("DEFAULT", "diff.toggle")] {
            assert!(build_rows(&rules, query).iter().any(|row| row.command == command), "{query}");
        }
        assert!(build_rows(&rules, "no such shortcut").is_empty());
    }

    #[test]
    fn written_text_is_what_remove_and_replace_send() {
        let rules = merge_with_defaults(vec![]);
        let mut written = Written::default();
        written.remember("diff.toggle", "Cmd+Shift+D", "!terminalFocus");
        let rule = Rule { command: "diff.toggle".into(), shortcut: crate::keybindings::parse_shortcut("Cmd+Shift+D").unwrap(), when: parse_when("!terminalFocus") };
        let mut rules = rules;
        rules.push(rule);
        let rows = build_rows(&rules, "");
        let row = rows.iter().find(|row| row.command == "diff.toggle" && row.key == "meta+shift+d").unwrap();
        assert_eq!(written.text_for(row), ("Cmd+Shift+D".to_owned(), "!terminalFocus".to_owned()));
        let default = rows.iter().find(|row| row.command == "diff.toggle" && row.key == "mod+d").unwrap();
        assert_eq!(written.text_for(default), ("mod+d".to_owned(), "!terminalFocus".to_owned()));
    }

    #[test]
    fn unknown_condition_names_are_called_out() {
        assert_eq!(unknown_when_variables("terminalFocus && wat"), ["wat"]);
        assert!(unknown_when_variables("composerFocus && turnRunning").is_empty());
        assert!(unknown_when_variables("(").is_empty());
    }
}
