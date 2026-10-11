//! Choosing several threads in the sidebar and the menu of what can be done to them all. Port of
//! `threadSelectionStore.ts` and the bulk menu of `Sidebar.tsx` (`handleMultiSelectContextMenu`,
//! `buildBulkTitleRegenerationContextMenuItem`, `buildBulkUnpinContextMenuItem`).
use std::collections::BTreeSet;

/// The rows chosen with Ctrl/Cmd+click and Shift+click, and where a Shift range starts from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub keys: BTreeSet<String>,
    pub anchor: Option<String>,
}
impl Selection {
    pub fn contains(&self, key: &str) -> bool {
        self.keys.contains(key)
    }
    pub fn has_selection(&self) -> bool {
        !self.keys.is_empty()
    }
    /// Ctrl/Cmd+click: the row joins or leaves; a row that joins becomes the anchor.
    pub fn toggle(&mut self, key: &str) {
        if !self.keys.remove(key) {
            self.keys.insert(key.to_owned());
            self.anchor = Some(key.to_owned());
        }
    }
    /// Shift+click: every row from the anchor to this one, in the order the sidebar lists them.
    /// With no anchor, or one that is not listed, the row alone joins and becomes the anchor.
    pub fn range_to(&mut self, key: &str, ordered: &[String]) {
        let anchor = self.anchor.clone();
        let positions = anchor.as_deref().and_then(|anchor| Some((ordered.iter().position(|candidate| candidate == anchor)?, ordered.iter().position(|candidate| candidate == key)?)));
        match positions {
            Some((from, to)) => {
                let (low, high) = (from.min(to), from.max(to));
                self.keys.extend(ordered[low..=high].iter().cloned());
            }
            None => {
                self.keys.insert(key.to_owned());
                self.anchor = Some(key.to_owned());
            }
        }
    }
    pub fn clear(&mut self) {
        if self.has_selection() || self.anchor.is_some() {
            *self = Self::default();
        }
    }
    pub fn set_anchor(&mut self, key: &str) {
        if self.anchor.as_deref() != Some(key) {
            self.anchor = Some(key.to_owned());
        }
    }
    pub fn remove(&mut self, keys: &[String]) {
        let before = self.keys.len();
        for key in keys {
            self.keys.remove(key);
        }
        if self.keys.len() != before && self.anchor.as_ref().is_some_and(|anchor| keys.contains(anchor)) {
            self.anchor = None;
        }
    }
}
/// What leaves the selection after a delete: what was deleted and what no longer exists, while
/// threads that failed or were not reached stay chosen.
pub fn deselect_after_delete(selected: &[String], deleted: &BTreeSet<String>, exists: impl Fn(&str) -> bool) -> Vec<String> {
    selected.iter().filter(|key| deleted.contains(*key) || !exists(key)).cloned().collect()
}

/// What the menu needs to know of one chosen thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub id: String,
    pub settled: bool,
    pub pinned: bool,
    pub can_snooze: bool,
    /// The server regenerates titles, and whether this thread's is being regenerated already.
    pub title_regeneration: bool,
    pub regenerating: bool,
}
/// What the server can do for threads at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Support {
    pub pinning: bool,
    pub settlement: bool,
    pub snooze: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Unpin,
    Settle,
    Snooze,
    RegenerateTitles,
    MarkUnread,
    Delete,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub action: Action,
    pub label: String,
    pub disabled: bool,
    pub destructive: bool,
}
/// The bulk menu. A count in a label is the number of threads the action touches: Unpin counts the
/// pinned ones and is absent when none is pinned, Settle counts the unsettled, Regenerate titles
/// the ones not already regenerating. Snooze appears only when every chosen thread can take it.
pub fn menu(selected: &[Facts], support: Support, can_operate: bool) -> Vec<Item> {
    let count = selected.len();
    if count == 0 {
        return vec![];
    }
    let item = |action, label: String, disabled: bool, destructive: bool| Item { action, label, disabled: disabled || !can_operate, destructive };
    let mut items = vec![];
    let pinned = selected.iter().filter(|thread| support.pinning && thread.pinned).count();
    if pinned > 0 {
        items.push(item(Action::Unpin, format!("Unpin ({pinned})"), false, false));
    }
    if support.settlement {
        let settling = selected.iter().filter(|thread| !thread.settled).count();
        items.push(item(Action::Settle, format!("Settle ({count})"), settling == 0, false));
    }
    if support.snooze && selected.iter().all(|thread| thread.can_snooze) {
        items.push(item(Action::Snooze, format!("Snooze ({count})"), false, false));
    }
    let supported = selected.iter().filter(|thread| thread.title_regeneration).count();
    if supported > 0 {
        let actionable = selected.iter().filter(|thread| thread.title_regeneration && !thread.regenerating).count();
        items.push(if actionable == 0 { item(Action::RegenerateTitles, format!("Regenerating\u{2026} ({supported})"), true, false) } else { item(Action::RegenerateTitles, format!("Regenerate titles ({actionable})"), false, false) });
    }
    // Reading state is local; any client may mark threads unread.
    items.push(Item { action: Action::MarkUnread, label: format!("Mark unread ({count})"), disabled: false, destructive: false });
    items.push(item(Action::Delete, format!("Delete ({count})"), false, true));
    items
}
/// The question before deleting several threads, in the original's words.
pub fn delete_message(count: usize) -> String {
    format!("Delete {count} thread{}?\nThis permanently clears conversation history for these threads.", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|key| (*key).to_owned()).collect()
    }
    fn facts(id: &str) -> Facts {
        Facts { id: id.into(), settled: false, pinned: false, can_snooze: true, title_regeneration: true, regenerating: false }
    }

    #[test]
    fn modifier_clicks_toggle_and_a_shift_click_extends_from_the_anchor() {
        let order = keys(&["a", "b", "c", "d", "e"]);
        let mut selection = Selection::default();
        selection.toggle("b");
        assert_eq!(selection.anchor.as_deref(), Some("b"));
        selection.range_to("d", &order);
        assert_eq!(selection.keys.iter().cloned().collect::<Vec<_>>(), keys(&["b", "c", "d"]));
        // The anchor stays, so the next shift-click extends from the same row, in either direction.
        selection.range_to("a", &order);
        assert_eq!(selection.keys.len(), 4);
        assert!(selection.contains("a") && selection.contains("d"));
        selection.toggle("c");
        assert!(!selection.contains("c"));
        assert_eq!(selection.anchor.as_deref(), Some("b"), "leaving does not move the anchor");
    }

    #[test]
    fn a_shift_click_without_a_usable_anchor_chooses_just_that_row() {
        let order = keys(&["a", "b"]);
        let mut selection = Selection::default();
        selection.range_to("b", &order);
        assert_eq!((selection.keys.len(), selection.anchor.as_deref()), (1, Some("b")));
        selection.anchor = Some("elsewhere".into());
        selection.range_to("a", &order);
        assert!(selection.contains("a") && selection.contains("b"));
        assert_eq!(selection.anchor.as_deref(), Some("a"));
    }

    #[test]
    fn removing_and_clearing() {
        let mut selection = Selection::default();
        selection.toggle("a");
        selection.toggle("b");
        selection.remove(&keys(&["b"]));
        assert_eq!(selection.anchor, None, "the anchor left with its row");
        assert!(selection.contains("a"));
        selection.clear();
        assert!(!selection.has_selection());
        let deleted: BTreeSet<String> = keys(&["a"]).into_iter().collect();
        assert_eq!(deselect_after_delete(&keys(&["a", "b", "c"]), &deleted, |key| key != "c"), keys(&["a", "c"]));
    }

    #[test]
    fn the_menu_counts_what_each_action_touches() {
        let mut pinned = facts("p");
        pinned.pinned = true;
        let mut settled = facts("s");
        settled.settled = true;
        let mut regenerating = facts("r");
        regenerating.regenerating = true;
        let support = Support { pinning: true, settlement: true, snooze: true };
        let items = menu(&[pinned.clone(), settled.clone(), regenerating, facts("x")], support, true);
        let labels: Vec<_> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, ["Unpin (1)", "Settle (4)", "Snooze (4)", "Regenerate titles (3)", "Mark unread (4)", "Delete (4)"]);
        assert!(items.last().unwrap().destructive);
        // Everything chosen is settled: nothing to settle. Nothing pinned: no Unpin.
        let items = menu(&[settled.clone()], support, true);
        assert!(items.iter().any(|item| item.action == Action::Settle && item.disabled));
        assert!(!items.iter().any(|item| item.action == Action::Unpin));
        // One that cannot be snoozed takes Snooze away from all.
        let mut blocked = facts("b");
        blocked.can_snooze = false;
        assert!(!menu(&[facts("x"), blocked], support, true).iter().any(|item| item.action == Action::Snooze));
        // All regenerating: a disabled line that says so.
        let mut busy = facts("r");
        busy.regenerating = true;
        let items = menu(&[busy], support, true);
        assert!(items.iter().any(|item| item.label == "Regenerating\u{2026} (1)" && item.disabled));
    }

    #[test]
    fn without_operate_access_only_marking_unread_stays_usable() {
        let items = menu(&[facts("a"), facts("b")], Support { pinning: true, settlement: true, snooze: true }, false);
        assert!(items.iter().all(|item| item.disabled == (item.action != Action::MarkUnread)));
        assert!(menu(&[], Support::default(), true).is_empty());
        assert_eq!(delete_message(1), "Delete 1 thread?\nThis permanently clears conversation history for these threads.");
        assert!(delete_message(3).starts_with("Delete 3 threads?"));
    }
}
