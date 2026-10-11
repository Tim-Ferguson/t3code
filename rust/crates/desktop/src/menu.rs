//! The application menu: the items and accelerators of `DesktopApplicationMenu.ts`, as data. `t3-ui` builds
//! the native menu from this and runs an `Action` when its item is chosen.
//!
//! Left out on purpose, with the reason: the original's Reload and Force Reload (the webview here refuses
//! any navigation after its first load, so a reload would only blank the window), and its Delete and Speech
//! entries (the native menu toolkit has no such roles).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Other,
}
impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") { Self::MacOs } else { Self::Other }
    }
}

/// What a chosen custom item does. The first three are the original's `open-settings`, `paste-as-text` and
/// the main window's zoom (`DesktopWindow.zoomMain`); the others are the menu items that need the shell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    OpenSettings,
    CheckForUpdates,
    PasteAsText,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleDevTools,
    /// Quit at once. The quit shortcut itself is not bound here: the page sees it, so that holding it can be required.
    Quit,
}
impl Action {
    pub const ALL: [Action; 8] = [Self::OpenSettings, Self::CheckForUpdates, Self::PasteAsText, Self::ZoomIn, Self::ZoomOut, Self::ZoomReset, Self::ToggleDevTools, Self::Quit];
    /// The item id the native menu reports back.
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenSettings => "open-settings",
            Self::CheckForUpdates => "check-for-updates",
            Self::PasteAsText => "paste-as-text",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
            Self::ZoomReset => "zoom-reset",
            Self::ToggleDevTools => "toggle-devtools",
            Self::Quit => "quit",
        }
    }
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.id() == id)
    }
}

/// The operating system's own items, which act on whatever has focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    About,
    Services,
    Hide,
    HideOthers,
    ShowAll,
    CloseWindow,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Fullscreen,
    Minimize,
    Maximize,
    BringAllToFront,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Item { action: Action, label: String, accelerator: Option<&'static str> },
    Role(Role),
    Separator,
    Submenu { label: String, entries: Vec<Entry> },
}

fn item(action: Action, label: &str, accelerator: Option<&'static str>) -> Entry {
    Entry::Item { action, label: label.to_owned(), accelerator }
}

/// The menu bar. On macOS the first menu is the application's own, with Settings in it; elsewhere Settings
/// and Quit are in File.
pub fn application_menu(platform: Platform, app_name: &str) -> Vec<Entry> {
    let mac = platform == Platform::MacOs;
    let settings = item(Action::OpenSettings, "Settings...", Some("CmdOrCtrl+,"));
    let mut menus = Vec::new();
    if mac {
        menus.push(Entry::Submenu {
            label: app_name.to_owned(),
            entries: vec![
                Entry::Role(Role::About),
                item(Action::CheckForUpdates, "Check for Updates...", None),
                Entry::Separator,
                settings.clone(),
                Entry::Separator,
                Entry::Role(Role::Services),
                Entry::Separator,
                Entry::Role(Role::Hide),
                Entry::Role(Role::HideOthers),
                Entry::Role(Role::ShowAll),
                Entry::Separator,
                item(Action::Quit, &format!("Quit {app_name}"), None),
            ],
        });
    }
    let mut file = Vec::new();
    if !mac {
        file.extend([settings, Entry::Separator]);
    }
    file.push(if mac { Entry::Role(Role::CloseWindow) } else { item(Action::Quit, "Quit", None) });
    menus.push(Entry::Submenu { label: "File".into(), entries: file });
    let edit = vec![
        Entry::Role(Role::Undo),
        Entry::Role(Role::Redo),
        Entry::Separator,
        Entry::Role(Role::Cut),
        Entry::Role(Role::Copy),
        Entry::Role(Role::Paste),
        item(Action::PasteAsText, "Paste as Text", Some("CmdOrCtrl+Shift+V")),
        Entry::Role(Role::SelectAll),
    ];
    menus.push(Entry::Submenu { label: "Edit".into(), entries: edit });
    menus.push(Entry::Submenu {
        label: "View".into(),
        entries: vec![
            item(Action::ToggleDevTools, "Toggle Developer Tools", Some(if mac { "Alt+Cmd+I" } else { "Ctrl+Shift+I" })),
            Entry::Separator,
            item(Action::ZoomReset, "Actual Size", Some("CmdOrCtrl+0")),
            item(Action::ZoomIn, "Zoom In", Some("CmdOrCtrl+=")),
            item(Action::ZoomOut, "Zoom Out", Some("CmdOrCtrl+-")),
            Entry::Separator,
            Entry::Role(Role::Fullscreen),
        ],
    });
    let mut window = vec![Entry::Role(Role::Minimize), Entry::Role(Role::Maximize)];
    if mac {
        window.extend([Entry::Separator, Entry::Role(Role::BringAllToFront)]);
    }
    menus.push(Entry::Submenu { label: "Window".into(), entries: window });
    menus.push(Entry::Submenu { label: "Help".into(), entries: vec![item(Action::CheckForUpdates, "Check for Updates...", None)] });
    menus
}

/// The zoom factor after a zoom action. The original steps Chromium's zoom level by 0.5 (`zoomMain`); a level
/// `n` is a factor of `1.2^n`. Kept in a range where the page stays usable.
pub fn zoomed(factor: f64, action: Action) -> f64 {
    const BASE: f64 = 1.2;
    let level = match action {
        Action::ZoomIn => factor.ln() / BASE.ln() + 0.5,
        Action::ZoomOut => factor.ln() / BASE.ln() - 0.5,
        Action::ZoomReset => 0.0,
        _ => return factor,
    };
    // Whole steps, so one step in and one step out come back to the same size.
    BASE.powf(((level * 2.0).round() / 2.0).clamp(-3.0, 5.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flatten(entries: &[Entry], into: &mut Vec<Entry>) {
        for entry in entries {
            if let Entry::Submenu { entries, .. } = entry {
                flatten(entries, into);
            } else {
                into.push(entry.clone());
            }
        }
    }
    fn items(platform: Platform) -> Vec<(Action, String, Option<&'static str>)> {
        let mut flat = Vec::new();
        flatten(&application_menu(platform, "T3 Code"), &mut flat);
        flat.into_iter().filter_map(|entry| if let Entry::Item { action, label, accelerator } = entry { Some((action, label, accelerator)) } else { None }).collect()
    }
    fn titles(platform: Platform) -> Vec<String> {
        application_menu(platform, "T3 Code").into_iter().filter_map(|entry| if let Entry::Submenu { label, .. } = entry { Some(label) } else { None }).collect()
    }

    #[test]
    fn macos_has_the_application_menu_first_with_settings_in_it_and_others_keep_settings_and_quit_in_file() {
        assert_eq!(titles(Platform::MacOs), ["T3 Code", "File", "Edit", "View", "Window", "Help"]);
        assert_eq!(titles(Platform::Other), ["File", "Edit", "View", "Window", "Help"]);
        let mac = application_menu(Platform::MacOs, "T3 Code");
        let Entry::Submenu { entries, .. } = &mac[0] else { panic!("the application menu is a submenu") };
        assert!(entries.contains(&item(Action::OpenSettings, "Settings...", Some("CmdOrCtrl+,"))));
        assert!(entries.contains(&item(Action::Quit, "Quit T3 Code", None)));
        let other = application_menu(Platform::Other, "T3 Code");
        let Entry::Submenu { entries, .. } = &other[0] else { panic!("file is a submenu") };
        assert_eq!(entries, &[item(Action::OpenSettings, "Settings...", Some("CmdOrCtrl+,")), Entry::Separator, item(Action::Quit, "Quit", None)]);
        // Close window is the macOS File menu's one entry; quitting is in the application menu there.
        let Entry::Submenu { entries, .. } = &mac[1] else { panic!("file is a submenu") };
        assert_eq!(entries, &[Entry::Role(Role::CloseWindow)]);
    }

    #[test]
    fn the_originals_accelerators_are_kept_and_no_two_items_share_one() {
        for platform in [Platform::MacOs, Platform::Other] {
            let items = items(platform);
            let accelerators: Vec<_> = items.iter().filter_map(|(_, _, accelerator)| *accelerator).collect();
            let mut unique = accelerators.clone();
            unique.sort_unstable();
            unique.dedup();
            assert_eq!(unique.len(), accelerators.len(), "{accelerators:?}");
            let accelerator = |wanted: Action| items.iter().find(|(action, _, _)| *action == wanted).and_then(|(_, _, accelerator)| *accelerator);
            assert_eq!(accelerator(Action::OpenSettings), Some("CmdOrCtrl+,"));
            assert_eq!(accelerator(Action::PasteAsText), Some("CmdOrCtrl+Shift+V"));
            assert_eq!(accelerator(Action::ZoomReset), Some("CmdOrCtrl+0"));
            assert_eq!(accelerator(Action::ZoomIn), Some("CmdOrCtrl+="));
            assert_eq!(accelerator(Action::ZoomOut), Some("CmdOrCtrl+-"));
        }
        // The same action can appear twice (update checks are in the application menu and in Help), but only
        // without an accelerator.
        for platform in [Platform::MacOs, Platform::Other] {
            let twice: Vec<_> = items(platform).into_iter().filter(|(action, ..)| *action == Action::CheckForUpdates).collect();
            assert!(twice.iter().all(|(_, _, accelerator)| accelerator.is_none()));
        }
    }

    #[test]
    fn every_action_in_the_menu_round_trips_through_its_id_and_every_id_is_distinct() {
        let mut ids: Vec<_> = Action::ALL.iter().map(|action| action.id()).collect();
        for action in Action::ALL {
            assert_eq!(Action::from_id(action.id()), Some(action));
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), Action::ALL.len());
        assert_eq!(Action::from_id("reload"), None);
        for platform in [Platform::MacOs, Platform::Other] {
            for (action, ..) in items(platform) {
                assert!(Action::ALL.contains(&action));
            }
        }
    }

    #[test]
    fn zoom_steps_in_whole_levels_returns_to_actual_size_and_stays_in_range() {
        let once = zoomed(1.0, Action::ZoomIn);
        assert!((once - 1.2f64.powf(0.5)).abs() < 1e-9);
        assert!((zoomed(once, Action::ZoomOut) - 1.0).abs() < 1e-9);
        assert!((zoomed(once, Action::ZoomReset) - 1.0).abs() < 1e-9);
        let mut factor = 1.0;
        for _ in 0..40 {
            factor = zoomed(factor, Action::ZoomIn);
        }
        assert!((factor - 1.2f64.powi(5)).abs() < 1e-9, "no further than five levels in");
        for _ in 0..40 {
            factor = zoomed(factor, Action::ZoomOut);
        }
        assert!((factor - 1.2f64.powi(-3)).abs() < 1e-9, "no further than three levels out");
        assert_eq!(zoomed(1.5, Action::OpenSettings), 1.5);
    }
}
