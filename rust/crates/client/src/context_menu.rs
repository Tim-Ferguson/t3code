//! The one model of a context menu, drawn by the web UI as a menu in the page and by the desktop app as the system's own menu.
//!
//! Port of `ContextMenuItem` (`packages/contracts/src/ipc.ts`), of the page's fallback menu (`apps/web/src/contextMenuFallback.ts`:
//! what it shows of an item) and of the desktop's native one (`apps/desktop/src/electron/ElectronMenu.ts`: what it keeps of an
//! item, where it puts separators). A menu is shown with the items and a position, and answers with the id of the leaf item
//! chosen, or none when it was dismissed (`showContextMenu`).
use serde::{Deserialize, Serialize};

/// One entry of a menu.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub disabled: bool,
    /// A label for the section below it, with no action. The page draws it; the system's menu does not (it has no such thing).
    #[serde(default)]
    pub header: bool,
    /// The name of the icon the page draws before the label (the system's menu draws none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// A divider immediately before this item.
    #[serde(default)]
    pub separator_before: bool,
    /// A check mark: the current choice among options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    /// A submenu. An item that has the field with no entries is left out of the system's menu and is a plain item on the page,
    /// as in the original, where `[]` is not `undefined`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Item>>,
}

impl Item {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), ..Self::default() }
    }
    pub fn header(label: impl Into<String>) -> Self {
        Self { header: true, ..Self::new("", label) }
    }
    pub fn icon(mut self, icon: &str) -> Self {
        self.icon = Some(icon.to_owned());
        self
    }
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
    pub fn separated(mut self) -> Self {
        self.separator_before = true;
        self
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }
    pub fn children(mut self, children: Vec<Item>) -> Self {
        self.children = Some(children);
        self
    }
    /// The children that can be chosen from, if the page opens a submenu for this item.
    pub fn submenu(&self) -> Option<&[Item]> {
        self.children.as_deref().filter(|children| !children.is_empty())
    }
    /// Whether the page draws the item as the destructive kind: a leaf that says so, or one that is called `delete`.
    pub fn looks_destructive(&self) -> bool {
        self.submenu().is_none() && (self.destructive || self.id == "delete")
    }
}

/// An item with the id of the leaf it answers with, found by id among the leaves (for tests and for callers that map answers).
pub fn find<'a>(items: &'a [Item], id: &str) -> Option<&'a Item> {
    items.iter().find_map(|item| if item.id == id { Some(item) } else { item.children.as_deref().and_then(|children| find(children, id)) })
}

/// What the system's menu is built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Native {
    Separator,
    /// A leaf: `index` is what the system reports when it is chosen; `choices` of the same `Native` list names the id.
    Leaf { index: usize, label: String, enabled: bool, checked: Option<bool> },
    Submenu { label: String, enabled: bool, entries: Vec<Native> },
}

/// The system's menu for the items, and the ids its leaves answer with, by index (`normalizeContextMenuItems` and `buildTemplate`).
/// Headers are dropped; a submenu with nothing left in it is dropped; a divider is never first and never doubled; the first
/// destructive item that does not start a section the caller marked gets a divider before it.
pub fn native(items: &[Item]) -> (Vec<Native>, Vec<String>) {
    let mut ids = Vec::new();
    let entries = native_entries(&normalized(items), &mut ids);
    (entries, ids)
}

fn normalized(source: &[Item]) -> Vec<Item> {
    let mut kept = Vec::new();
    for item in source {
        if item.header {
            continue;
        }
        let mut item = item.clone();
        if let Some(children) = &item.children {
            let children = normalized(children);
            if children.is_empty() {
                continue;
            }
            item.children = Some(children);
        }
        kept.push(item);
    }
    kept
}

fn native_entries(items: &[Item], ids: &mut Vec<String>) -> Vec<Native> {
    let mut template: Vec<Native> = Vec::new();
    let mut destructive_divided = false;
    let mut explicit_section = false;
    fn divide(template: &mut Vec<Native>) {
        if template.is_empty() || template.last() == Some(&Native::Separator) {
            return;
        }
        template.push(Native::Separator);
    }
    for item in items {
        if item.separator_before {
            divide(&mut template);
            explicit_section = true;
        }
        if item.destructive && !destructive_divided && !explicit_section && !template.is_empty() {
            divide(&mut template);
            destructive_divided = true;
        }
        match item.children.as_deref().filter(|children| !children.is_empty()) {
            Some(children) => template.push(Native::Submenu { label: item.label.clone(), enabled: !item.disabled, entries: native_entries(children, ids) }),
            None => {
                ids.push(item.id.clone());
                template.push(Native::Leaf { index: ids.len() - 1, label: item.label.clone(), enabled: !item.disabled, checked: item.checked });
            }
        }
    }
    template
}

/// Where the system's menu opens, in the window's points, from a position in the page's pixels at its zoom: the original multiplies
/// by the zoom factor and floors, and shows the menu at the pointer when the position is missing, negative or not a number.
pub fn native_position(position: Option<(f64, f64)>, zoom: f64) -> Option<(f64, f64)> {
    let (x, y) = position?;
    (x.is_finite() && y.is_finite() && x >= 0.0 && y >= 0.0 && zoom.is_finite()).then(|| ((x * zoom).floor(), (y * zoom).floor()))
}

// ---------------------------------------------------------------------------------------------------------------------------------
// The items of the menus the app shows. Each is the original's list with its ids; what a choice does is the caller's.
// ---------------------------------------------------------------------------------------------------------------------------------

/// What the per-thread menu depends on (`ThreadActionMenuState` of `threadActionMenu.logic.ts`).
#[derive(Clone, Debug, Default)]
pub struct ThreadActions {
    pub can_operate: bool,
    pub branch: Option<String>,
    /// Project scoping for the thread list: the project's name and whether the list is already scoped to it. None where nothing
    /// is scoped by the menu.
    pub project_filter: Option<(String, bool)>,
    pub pinned: bool,
    pub settled: bool,
    pub auto_settle_enabled: bool,
    pub snoozed: bool,
    pub can_snooze_now: bool,
    pub regenerating_title: bool,
    /// Archiving refuses a thread with an attached provider, so the item is disabled rather than left to fail.
    pub running: bool,
    pub settlement: bool,
    pub auto_settle_opt_out: bool,
    pub snooze: bool,
    pub pinning: bool,
    pub title_regeneration: bool,
    /// Presets as (id, label, when) for `snooze:<id>`.
    pub snooze_presets: Vec<(String, String, String)>,
}

/// Actions that need the right to operate (`threadActionRequiresOperate`): navigation, read markers and copying do not.
pub fn thread_action_requires_operate(id: &str) -> bool {
    !matches!(id, "new-thread-on-branch" | "project-settings" | "mark-unread" | "copy" | "copy-path" | "copy-branch" | "copy-thread-id")
}

/// The per-thread menu of the sidebar row and the chat header (`buildThreadActionMenuItems`): one order and one set of gates for both.
pub fn thread_actions(state: &ThreadActions) -> Vec<Item> {
    let mut items = Vec::new();
    if let Some(branch) = &state.branch {
        items.push(Item::new("new-thread-on-branch", format!("New thread on {branch}")).icon("message-square-plus"));
    }
    if state.pinning {
        items.push(if state.pinned { Item::new("unpin", "Unpin thread").icon("pin-off") } else { Item::new("pin", "Pin thread").icon("pin") });
    }
    // Both lifecycle actions stay available on pinned threads: settling clears the pin, and snoozing hides the card with the pin intact.
    if state.settlement {
        items.push(if state.settled { Item::new("unsettle", "Un-settle thread").icon("circle-check") } else { Item::new("settle", "Settle thread").icon("circle-check") });
    }
    if state.snooze {
        items.push(if state.snoozed {
            Item::new("unsnooze", "Wake thread").icon("clock")
        } else {
            let mut children: Vec<Item> = state.snooze_presets.iter().map(|(id, label, when)| Item::new(format!("snooze:{id}"), format!("{label} ({when})"))).collect();
            children.push(Item::new("snooze:custom", "Custom\u{2026}").separated());
            Item::new("snooze", "Snooze").icon("clock").disabled(!state.can_snooze_now).children(children)
        });
    }
    items.push(Item::new("rename", "Rename thread").icon("pencil").separated());
    if state.title_regeneration {
        items.push(Item::new("regenerate-title", if state.regenerating_title { "Regenerating\u{2026}" } else { "Regenerate title" }).icon("refresh-cw").disabled(state.regenerating_title));
    }
    items.push(Item::new("mark-unread", "Mark unread").icon("mail-open"));
    if let Some((name, active)) = &state.project_filter {
        items.push(Item::new("filter-by-project", if *active { "Show all projects".to_owned() } else { format!("Filter by {name}") }).icon("folder-tree"));
    }
    // A setting with the current choice checked, not a one-shot action.
    if state.auto_settle_opt_out {
        items.push(Item::new("auto-settle", "Auto-settle behavior").icon("timer").children(vec![
            Item::new("auto-settle:enabled", "Enabled").checked(state.auto_settle_enabled),
            Item::new("auto-settle:disabled", "Disabled").checked(!state.auto_settle_enabled),
        ]));
    }
    let mut copy = vec![Item::new("copy-path", "Path").icon("folder")];
    if state.branch.is_some() {
        copy.push(Item::new("copy-branch", "Branch").icon("git-branch"));
    }
    copy.push(Item::new("copy-thread-id", "Thread ID").icon("hash"));
    items.push(Item::new("copy", "Copy").icon("copy").separated().children(copy));
    items.push(Item::new("project-settings", "Project settings").icon("settings"));
    items.push(Item::new("archive", "Archive thread").icon("archive").disabled(state.running).separated());
    items.push(Item::new("delete", "Delete").icon("trash").destructive());
    if state.can_operate {
        return items;
    }
    items
        .into_iter()
        .map(|mut item| {
            if thread_action_requires_operate(&item.id) {
                item.disabled = true;
                if let Some(children) = &mut item.children {
                    for child in children {
                        child.disabled = true;
                    }
                }
            }
            item
        })
        .collect()
}

/// What a file's menu can offer, from the server's configuration (`FileContextMenuCapabilities`).
#[derive(Clone, Debug, Default)]
pub struct FileCapabilities {
    /// "Reveal in Finder" and the like, in the server's wording; none when the server cannot reveal.
    pub reveal_label: Option<String>,
    pub can_open_default: bool,
    pub editor_ids: Vec<String>,
}

/// "Reveal in Finder" by the environment's operating system (`revealInFileExplorerLabelForOs`), or by what the server says it reveals in
/// (`revealInFileExplorerLabelForKind`): on WSL the reveal can run through Windows File Explorer though the host reports Linux.
pub fn reveal_label(os: Option<&str>, kind: Option<&str>) -> &'static str {
    match kind {
        Some("finder") => "Reveal in Finder",
        Some("file-explorer") => "Reveal in File Explorer",
        Some(_) => "Reveal in Files",
        None => match os {
            Some("darwin") => "Reveal in Finder",
            Some("windows") => "Reveal in File Explorer",
            _ => "Reveal in Files",
        },
    }
}

/// The menu of a workspace file: open it with the default app, reveal it, or open it with one of the detected editors
/// (`buildFileContextMenuItems`). Empty without a path on the machine the server runs on, and when nothing can act.
pub fn file(has_absolute_path: bool, capabilities: &FileCapabilities) -> Vec<Item> {
    if !has_absolute_path {
        return vec![];
    }
    let mut items = Vec::new();
    if capabilities.can_open_default {
        items.push(Item::new("open", "Open").icon("pencil"));
    }
    if let Some(label) = &capabilities.reveal_label {
        items.push(Item::new("reveal-in-folder", label.clone()).icon("folder-tree"));
    }
    let editors: Vec<Item> = capabilities.editor_ids.iter().filter(|id| *id != "file-manager").map(|id| Item::new(format!("editor:{id}"), crate::editors::label(id).unwrap_or(id).to_owned())).collect();
    if !editors.is_empty() {
        items.push(Item::new("open-with", "Open with").children(editors));
    }
    items
}

/// The menu of a tab in the panel beside the conversation (`handleTabContextMenu`): copy the file's path, and the four ways to close.
pub fn tab(copy_path: bool, index: usize, count: usize) -> Vec<Item> {
    let mut items = Vec::new();
    if copy_path {
        items.push(Item::new("copy-path", "Copy path"));
    }
    items.push(Item::new("close", "Close"));
    items.push(Item::new("close-others", "Close others").disabled(count <= 1));
    items.push(Item::new("close-to-right", "Close to the right").disabled(index + 1 >= count));
    items.push(Item::new("close-all", "Close all").disabled(count == 0));
    items
}

/// What the tabs left open are after a close choice: the paths that stay, in order. `close` of the clicked tab alone is
/// `["close"]`. None when `choice` is not a close.
pub fn after_close(choice: &str, paths: &[String], index: usize) -> Option<Vec<String>> {
    Some(match choice {
        "close" => paths.iter().enumerate().filter(|(at, _)| *at != index).map(|(_, path)| path.clone()).collect(),
        "close-others" => paths.get(index).cloned().into_iter().collect(),
        "close-to-right" => paths.iter().take(index + 1).cloned().collect(),
        "close-all" => vec![],
        _ => return None,
    })
}

/// The menu of an archived thread (`handleArchivedThreadContextMenu`).
pub fn archived_thread() -> Vec<Item> {
    vec![Item::new("unarchive", "Unarchive"), Item::new("delete", "Delete").destructive()]
}

/// Where a picture or video came from, as the media menu offers it (`MediaReference`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaReference {
    File { path: String, relative_path: Option<String> },
    Url(String),
}

/// The menu of a picture or video in a message (`MediaActions.tsx`): copy where it came from, open it in the file viewer, save it,
/// copy the picture. `noun` is "image" or "video"; `unavailable` is a source that cannot be read now.
pub fn media(noun: &str, reference: Option<&MediaReference>, can_open_file: bool, unavailable: bool, can_copy_image: bool) -> Vec<Item> {
    let mut items = Vec::new();
    match reference {
        Some(MediaReference::File { relative_path, .. }) => {
            items.push(Item::new("copy-full-path", "Copy full path"));
            if relative_path.is_some() {
                items.push(Item::new("copy-relative-path", "Copy relative path"));
            }
        }
        Some(MediaReference::Url(_)) => items.push(Item::new("copy-url", "Copy URL")),
        None => {}
    }
    if can_open_file {
        items.push(Item::new("open-file", "Open in file viewer").disabled(unavailable));
    }
    items.push(Item::new("save", format!("Save {noun}")).disabled(unavailable));
    if noun == "image" {
        items.push(Item::new("copy-image", "Copy image").disabled(unavailable || !can_copy_image));
    }
    items
}

/// A menu of one copy: "Copy full path" over a workspace, "Copy branch name" over a branch.
pub fn copy_only(id: &str, label: &str) -> Vec<Item> {
    vec![Item::new(id, label).icon("copy")]
}

/// The menu of a link in a message (`externalLinkContextMenuItems`). The integrated browser is not always there to offer, and the
/// other answers hold wherever a link does.
pub fn external_link(can_open_in_preview: bool, thread_link: Option<ThreadLink>) -> Vec<Item> {
    let mut items = Vec::new();
    match thread_link {
        Some(ThreadLink::Link) => items.push(Item::new("link-to-thread", "Link to thread")),
        Some(ThreadLink::Unlink) => items.push(Item::new("unlink-from-thread", "Unlink from thread")),
        None => {}
    }
    if can_open_in_preview {
        items.push(Item::new("open-in-preview", "Open in integrated browser"));
    }
    items.push(Item::new("open-external", "Open in system browser"));
    items.push(Item::new("copy-link", "Copy Link"));
    items
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadLink {
    Link,
    Unlink,
}

/// The right-click on a change request's number (`pullRequestLinkContextMenuItems`): copy first, since it is the reason to right-click.
pub fn pull_request_link(open_label: &str) -> Vec<Item> {
    vec![Item::new("copy-link", "Copy link").icon("copy"), Item::new("open-external", open_label)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(entries: &[Native]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                Native::Separator => "-".to_owned(),
                Native::Leaf { label, enabled, checked, .. } => format!("{label}{}{}", if *enabled { "" } else { " (off)" }, match checked { Some(true) => " [x]", Some(false) => " [ ]", None => "" }),
                Native::Submenu { label, entries, .. } => format!("{label} > ({})", labels(entries).join(", ")),
            })
            .collect()
    }

    #[test]
    fn the_system_menu_drops_headers_and_empty_submenus_and_divides_where_the_original_does() {
        let items = vec![
            Item::header("Section"),
            Item::new("a", "A"),
            Item::new("empty", "Empty").children(vec![]),
            Item::new("only-headers", "Only headers").children(vec![Item::header("Nothing")]),
            Item::new("b", "B").separated(),
            Item::new("c", "C").disabled(true).checked(true),
            Item::new("d", "D").destructive(),
            Item::new("e", "E").destructive(),
        ];
        let (entries, ids) = native(&items);
        // A header never reaches the system. The destructive divider is not added once a section was started by hand: the
        // original's flag is set by any `separatorBefore`.
        assert_eq!(labels(&entries), ["A", "-", "B", "C (off) [x]", "D", "E"]);
        assert_eq!(ids, ["a", "b", "c", "d", "e"]);
        let (entries, _) = native(&[Item::new("a", "A"), Item::new("d", "D").destructive(), Item::new("e", "E").destructive()]);
        assert_eq!(labels(&entries), ["A", "-", "D", "E"]);
        // A divider is never first.
        let (entries, _) = native(&[Item::new("a", "A").separated(), Item::new("b", "B")]);
        assert_eq!(labels(&entries), ["A", "B"]);
        // Each submenu begins its own section.
        let (entries, ids) = native(&[Item::new("m", "More").children(vec![Item::new("x", "X"), Item::new("y", "Y").destructive()]), Item::new("z", "Z")]);
        assert_eq!(labels(&entries), ["More > (X, -, Y)", "Z"]);
        assert_eq!(ids, ["x", "y", "z"]);
        assert!(matches!(&entries[1], Native::Leaf { index: 2, .. }));
    }

    #[test]
    fn a_position_is_scaled_by_the_zoom_and_floored_or_left_to_the_pointer() {
        assert_eq!(native_position(Some((10.7, 20.2)), 1.0), Some((10.0, 20.0)));
        assert_eq!(native_position(Some((100.0, 50.0)), 1.5), Some((150.0, 75.0)));
        assert_eq!(native_position(Some((-1.0, 5.0)), 1.0), None);
        assert_eq!(native_position(Some((f64::NAN, 5.0)), 1.0), None);
        assert_eq!(native_position(None, 1.0), None);
    }

    fn actions() -> ThreadActions {
        ThreadActions {
            can_operate: true,
            branch: Some("feature/x".into()),
            project_filter: Some(("Widgets".into(), false)),
            auto_settle_enabled: true,
            can_snooze_now: true,
            settlement: true,
            auto_settle_opt_out: true,
            snooze: true,
            pinning: true,
            title_regeneration: true,
            snooze_presets: vec![("later".into(), "Later today".into(), "5:00 PM".into())],
            ..ThreadActions::default()
        }
    }

    #[test]
    fn the_thread_menu_is_the_originals_list_in_its_order() {
        let items = thread_actions(&actions());
        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["new-thread-on-branch", "pin", "settle", "snooze", "rename", "regenerate-title", "mark-unread", "filter-by-project", "auto-settle", "copy", "project-settings", "archive", "delete"]);
        let snooze = find(&items, "snooze").unwrap();
        let children: Vec<(&str, &str)> = snooze.children.as_ref().unwrap().iter().map(|child| (child.id.as_str(), child.label.as_str())).collect();
        assert_eq!(children, [("snooze:later", "Later today (5:00 PM)"), ("snooze:custom", "Custom\u{2026}")]);
        let copy: Vec<&str> = find(&items, "copy").unwrap().children.as_ref().unwrap().iter().map(|child| child.id.as_str()).collect();
        assert_eq!(copy, ["copy-path", "copy-branch", "copy-thread-id"]);
        // The current auto-settle choice is the checked one.
        assert_eq!(find(&items, "auto-settle:enabled").unwrap().checked, Some(true));
        assert_eq!(find(&items, "auto-settle:disabled").unwrap().checked, Some(false));
        assert!(find(&items, "delete").unwrap().looks_destructive() && !find(&items, "archive").unwrap().looks_destructive());
        // What a thread is now decides the wording and the id of the toggles.
        let other = thread_actions(&ThreadActions { pinned: true, settled: true, snoozed: true, regenerating_title: true, project_filter: Some(("Widgets".into(), true)), branch: None, ..actions() });
        assert!(find(&other, "unpin").is_some() && find(&other, "unsettle").is_some() && find(&other, "unsnooze").is_some());
        assert_eq!(find(&other, "regenerate-title").map(|item| (item.label.as_str(), item.disabled)), Some(("Regenerating\u{2026}", true)));
        assert_eq!(find(&other, "filter-by-project").unwrap().label, "Show all projects");
        assert!(find(&other, "new-thread-on-branch").is_none() && find(&other, "copy-branch").is_none());
    }

    #[test]
    fn without_the_right_to_operate_only_navigation_read_markers_and_copying_stay_available() {
        let items = thread_actions(&ThreadActions { can_operate: false, ..actions() });
        for item in &items {
            assert_eq!(item.disabled, thread_action_requires_operate(&item.id), "{}", item.id);
        }
        assert!(!find(&items, "mark-unread").unwrap().disabled);
        assert!(!find(&items, "copy-path").unwrap().disabled);
        assert!(find(&items, "snooze:later").unwrap().disabled && find(&items, "auto-settle:enabled").unwrap().disabled);
        // The system's menu keeps them, switched off; the copy submenu stays whole.
        let (entries, _) = native(&items);
        assert!(labels(&entries).iter().any(|label| label.starts_with("Pin thread (off)")));
    }

    #[test]
    fn a_tabs_menu_closes_the_way_the_originals_does() {
        let items = tab(true, 1, 3);
        let ids: Vec<&str> = items.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, ["copy-path", "close", "close-others", "close-to-right", "close-all"]);
        assert!(items.iter().all(|item| !item.disabled));
        // The only tab has no others, and the last has nothing to its right.
        assert!(find(&tab(false, 0, 1), "close-others").unwrap().disabled);
        assert!(find(&tab(false, 2, 3), "close-to-right").unwrap().disabled && find(&tab(false, 2, 3), "copy-path").is_none());
        let paths: Vec<String> = ["a", "b", "c"].iter().map(|path| (*path).to_owned()).collect();
        assert_eq!(after_close("close", &paths, 1).unwrap(), ["a", "c"]);
        assert_eq!(after_close("close-others", &paths, 1).unwrap(), ["b"]);
        assert_eq!(after_close("close-to-right", &paths, 1).unwrap(), ["a", "b"]);
        assert!(after_close("close-all", &paths, 1).unwrap().is_empty() && after_close("copy-path", &paths, 1).is_none());
    }

    #[test]
    fn a_pictures_menu_offers_where_it_came_from_then_saving_and_copying() {
        let ids = |items: Vec<Item>| items.into_iter().map(|item| (item.id, item.disabled)).collect::<Vec<_>>();
        let file = MediaReference::File { path: "/work/a.png".into(), relative_path: Some("a.png".into()) };
        assert_eq!(
            ids(media("image", Some(&file), true, false, true)),
            [("copy-full-path".to_owned(), false), ("copy-relative-path".to_owned(), false), ("open-file".to_owned(), false), ("save".to_owned(), false), ("copy-image".to_owned(), false)]
        );
        // A video has no picture to copy; a source that cannot be read now cannot be saved, opened or copied.
        let video = ids(media("video", Some(&MediaReference::Url("https://x.invalid/v.mp4".into())), false, true, true));
        assert_eq!(video, [("copy-url".to_owned(), false), ("save".to_owned(), true)]);
        let blocked = media("image", None, true, true, true);
        assert!(blocked.iter().all(|item| item.disabled));
        assert!(media("image", None, false, false, false).iter().any(|item| item.id == "copy-image" && item.disabled));
        assert_eq!(media("video", None, false, false, false)[0].label, "Save video");
    }

    #[test]
    fn a_links_menu_keeps_what_can_be_honoured() {
        let ids = |items: Vec<Item>| items.into_iter().map(|item| item.id).collect::<Vec<_>>();
        assert_eq!(ids(external_link(true, None)), ["open-in-preview", "open-external", "copy-link"]);
        assert_eq!(ids(external_link(false, None)), ["open-external", "copy-link"]);
        assert_eq!(ids(external_link(false, Some(ThreadLink::Link))), ["link-to-thread", "open-external", "copy-link"]);
        assert_eq!(ids(external_link(true, Some(ThreadLink::Unlink)))[0], "unlink-from-thread");
        assert_eq!(pull_request_link("Open on GitLab")[1].label, "Open on GitLab");
    }
}
