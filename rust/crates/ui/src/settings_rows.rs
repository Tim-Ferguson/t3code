//! The rows of the General page that are a single value: a switch, or a choice among a few. A client
//! setting is read from and written to this device's preferences (`client_settings::Writer`); an
//! environment setting is one `server.updateSettings` patch, sent by the page (`general_page`), which
//! also decides whether the person may send it. Titles, descriptions and defaults are the original's
//! (`SettingsPanels.tsx`, `packages/contracts/src/settings.ts`).
//!
//! Only settings something in this UI or on the server acts on are listed: a switch that changes
//! nothing would be a lie.
use crate::{
    client_settings::Writer,
    runtime::{UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};

/// A choice's options: the JSON value it stores and the label the person reads.
pub(crate) type Options = &'static [(&'static str, &'static str)];

/// One row's meaning, whichever store holds the value.
#[derive(Clone, Copy, PartialEq)]
pub(crate) struct Row {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// The JSON the original's default decodes to.
    pub default: &'static str,
    /// Empty for a switch.
    pub options: Options,
    /// A slider's smallest, largest and step, and the unit written after its value.
    pub range: Option<(i64, i64, i64, &'static str)>,
}
impl Row {
    pub const fn switch(key: &'static str, title: &'static str, description: &'static str, default: bool) -> Self {
        Self { key, title, description, default: if default { "true" } else { "false" }, options: &[], range: None }
    }
    pub const fn choice(key: &'static str, title: &'static str, description: &'static str, default: &'static str, options: Options) -> Self {
        Self { key, title, description, default, options, range: None }
    }
    pub const fn slider(key: &'static str, title: &'static str, description: &'static str, default: &'static str, min: i64, max: i64, step: i64, unit: &'static str) -> Self {
        Self { key, title, description, default, options: &[], range: Some((min, max, step, unit)) }
    }
    fn is_default(self, value: &Value) -> bool {
        serde_json::from_str::<Value>(self.default).is_ok_and(|default| default == *value)
    }
}

/// The rows that follow this device's preferences.
pub(crate) const BEHAVIOR: [Row; 8] = [
    Row::choice("sendShortcut", "Send shortcut", "Choose when Enter sends a prompt or inserts a new line", "\"enter\"", &[("\"enter\"", "Enter sends"), ("\"mod-enter-multiline\"", "Mod+Enter sends, Enter adds a line"), ("\"mod-enter\"", "Mod+Enter sends")]),
    Row::choice("followUpBehavior", "Follow-up behavior", "What a message sent while a run is going does. Queue waits for the run to end; steer redirects it now.", "\"queue\"", &[("\"queue\"", "Queue"), ("\"steer\"", "Steer")]),
    Row::switch("showSkillsInSlashMenu", "Show skills in slash menu", "Also include skills in the / command menu. Skills always appear when you type $.", true),
    Row::switch("planModeEnabled", "Plan mode (legacy)", "Restore Build/Plan, /plan, /default, and Shift+Tab. Off uses build mode.", false),
    Row::switch("inAppNotificationsEnabled", "In-app notifications", "Show a toast when another thread finishes, fails, or needs input or approval while this app has focus.", false),
    Row::switch("proactivePanelsEnabled", "Proactive panels", "Open linked pull requests first. Otherwise, open Changes for edits to at least 3 files or 50 lines.", false),
    Row::switch("composerCollapseOnScroll", "Collapse composer on scroll", "Rest the composer of an existing thread into a single line when you scroll the conversation. Focus the composer or start typing to expand it again.", true),
    Row::switch("sidebarWorkingShelfEnabled", "Working section (beta)", "Fold working and monitoring threads into a Working section. They return to the top of the inbox when they need you.", false),
];
/// How a thread that finishes or needs you is announced outside the window. Changing it asks the system for permission first.
pub(crate) const NOTIFICATION_MODE: Row = Row::choice("notificationMode", "Thread notifications", t3_client::thread_notifications::DESCRIPTION, "\"off\"", &t3_client::thread_notifications::MODE_OPTIONS);
pub(crate) const CONFIRMATIONS: [Row; 3] = [
    Row::switch("confirmThreadArchive", "Archive confirmation", "Require a second click on the inline archive action before a thread is archived.", false),
    Row::switch("confirmThreadDelete", "Delete confirmation", "Ask before deleting a thread and its chat history.", true),
    Row::switch("confirmThreadUnpin", "Unpin confirmation", "Ask before unpinning a thread from the pinned section.", false),
];
/// The desktop app's quit shortcut: held, pressed twice, or acted on at once (`quit_hold`). Only the desktop app has a window to quit, so
/// only its General page lists it.
pub(crate) const QUIT: [Row; 1] = [Row::choice("confirmQuit", "Quit shortcut", "Hold mode also quits on two quick presses.", "\"hold\"", &[("\"direct\"", "Direct"), ("\"hold\"", "Hold"), ("\"double-click\"", "Double press")])];
/// Display preferences of the General page.
pub(crate) const DISPLAY: [Row; 3] = [
    Row::choice("sidebarProjectSortOrder", "Project order", "Order of projects in the sidebar project picker and command palette.", "\"updated_at\"", &[("\"updated_at\"", "Last user message"), ("\"created_at\"", "Created at"), ("\"manual\"", "Manual")]),
    Row::choice("timestampFormat", "Time format", "System default follows your browser or OS clock preference.", "\"locale\"", &[("\"locale\"", "System default"), ("\"12-hour\"", "12-hour"), ("\"24-hour\"", "24-hour")]),
    Row::switch("contextWindowMeterEnabled", "Context window indicator (legacy)", "Shows context window usage as a circular indicator in the composer.", false),
];
pub(crate) const DIFFS: [Row; 3] = [
    Row::choice("diffLayout", "Diff layout", "Show diffs stacked or side by side. The toggle in the diff toolbar changes this too.", "\"stacked\"", &[("\"stacked\"", "Stacked"), ("\"split\"", "Split")]),
    Row::switch("diffIgnoreWhitespace", "Hide whitespace changes", "Set whether the diff panel ignores whitespace-only edits by default.", true),
    Row::choice("diffFilesCollapsed", "Default diff file state", "Start with files expanded or collapsed when opening diffs or a pull request's Code tab.", "true", &[("true", "Collapsed"), ("false", "Expanded")]),
];
/// The rows of the Appearance page that follow this device's preferences.
pub(crate) const LAYOUT: [Row; 6] = [
    Row::slider("appearanceContrast", "Contrast", "Adjust the contrast of colors and borders across the interface.", "100", 50, 200, 5, "%"),
    Row::slider("glassOpacity", "Glass opacity", "Higher values make menus, dialogs, and the composer more solid.", "80", 40, 100, 5, "%"),
    Row::slider("panelAnimationDurationMs", "Panel animations", "Set how fast panels open and close.", "0", 0, 400, 50, " ms"),
    Row::switch("persistComposerContextStrip", "Composer context", "Keep branch and worktree controls below the composer after a thread starts.", false),
    Row::choice("chatWidth", "Chat width", "Set how wide messages and the composer can grow on large screens.", "\"comfortable\"", &[("\"comfortable\"", "Comfortable"), ("\"wide\"", "Wide"), ("\"full\"", "Full width")]),
    Row::choice("diffColorScheme", "Diff colors", "Choose colors for additions and deletions, including change counts.", "\"red-green\"", &[("\"red-green\"", "Red and green"), ("\"blue-orange\"", "Blue and orange")]),
];
/// How a Dev or Nightly environment is marked in the sidebar (`stage_art`); the row is offered only where the environment has a mark.
pub(crate) const ENVIRONMENT_IDENTIFICATION: [Row; 1] = [Row::choice("environmentIdentificationMode", "Environment identification", "Choose how Dev and Nightly environments are identified.", "\"artwork\"", &[("\"artwork\"", "Artwork"), ("\"pill\"", "Version pill"), ("\"none\"", "None")])];
/// The rows that follow the environment's settings, by the key of the server's settings document.
pub(crate) const ENVIRONMENT: [Row; 4] = [
    Row::choice("responseStreamingMode", "Response streaming", "Show a reply as it is written, paragraph by paragraph, or once the turn ends.", "\"paragraph\"", &[("\"paragraph\"", "By paragraph"), ("\"turn\"", "When the turn ends")]),
    Row::switch("enableProviderUpdateChecks", "Provider update checks", "Check installed provider CLIs for newer available versions.", true),
    Row::switch("continueThreadsAfterServerUpdate", "Continue threads after restarts", "Automatically resume interrupted threads after an update, crash, or machine restart on the selected environments.", false),
    Row::switch("newWorktreesStartFromOrigin", "Start from origin", "Creates the worktree from the latest matching branch on origin instead of your local branch.", true),
];

/// The control and its reset, for a value `current`; `change` receives the JSON the person chose.
/// A row's label names the control (`aria-label` is the title) so a test and a screen reader find it.
pub(crate) fn control(row: Row, current: &Value, disabled: bool, change: EventHandler<Value>) -> Element {
    let at_default = row.is_default(current);
    rsx! {
        div { class: "setting-row", key: "{row.key}", "data-setting": "{row.key}",
            div { class: "font-description", strong { "{row.title}" } p { class: "muted", "{row.description}" } }
            div { class: "font-controls",
                if !at_default {
                    button { r#type: "button", "aria-label": "Reset {row.title}", disabled, onclick: move |_| {
                        if let Ok(default) = serde_json::from_str::<Value>(row.default) { change.call(default); }
                    }, "Reset" }
                }
                if let Some((min, max, step, unit)) = row.range {
                    output { class: "setting-value", "{current}{unit}" }
                    input { r#type: "range", "aria-label": "{row.title}", min: "{min}", max: "{max}", step: "{step}", value: "{current}", disabled,
                        onchange: move |event| { if let Ok(number) = event.value().parse::<i64>() { change.call(Value::from(number)); } } }
                } else if row.options.is_empty() {
                    input { r#type: "checkbox", role: "switch", "aria-label": "{row.title}", checked: *current == Value::Bool(true), disabled, onchange: move |event| change.call(Value::Bool(event.checked())) }
                } else {
                    select { "aria-label": "{row.title}", disabled, value: "{current}", onchange: move |event| {
                        if let Ok(value) = serde_json::from_str::<Value>(&event.value()) { change.call(value); }
                    },
                        for (value, label) in row.options.iter() { option { key: "{value}", value: "{value}", selected: current.to_string() == *value, "{label}" } }
                    }
                }
            }
        }
    }
}

/// One change to this device's preferences. Without a writer (a document nothing persists, as in a
/// test) the change applies to what the app holds.
pub(crate) fn patch(state: Store<UiModel>, change: Value) {
    if let Some(writer) = try_consume_context::<Writer>() {
        writer.patch(state, change);
        return;
    }
    let mut merged = serde_json::to_value(&*state.client_settings().peek()).unwrap_or(Value::Null);
    if let (Some(merged), Some(change)) = (merged.as_object_mut(), change.as_object()) {
        merged.extend(change.clone());
    }
    if let Ok(next) = serde_json::from_value(merged) {
        state.client_settings().set(next);
    }
}

/// A group of this device's preferences.
#[component]
pub(crate) fn ClientRows(state: Store<UiModel>, rows: &'static [Row]) -> Element {
    let document = serde_json::to_value(&*state.client_settings().read()).unwrap_or(Value::Null);
    rsx! {
        for row in rows.iter().copied() {
            { control(row, &document[row.key], false, {
                EventHandler::new(move |value: Value| patch(state, json!({ row.key: value })))
            }) }
        }
    }
}

/// Every row that follows this device's preferences, for "Restore device defaults".
const DEVICE: [&[Row]; 8] = [&[NOTIFICATION_MODE], &BEHAVIOR, &CONFIRMATIONS, &QUIT, &DIFFS, &LAYOUT, &ENVIRONMENT_IDENTIFICATION, &DISPLAY];

/// The titles of the device preferences that are not at their defaults.
pub(crate) fn changed_labels(state: Store<UiModel>) -> Vec<&'static str> {
    let document = serde_json::to_value(&*state.client_settings().read()).unwrap_or(Value::Null);
    DEVICE.iter().flat_map(|rows| rows.iter()).filter(|row| !row.is_default(&document[row.key])).map(|row| row.title).collect()
}
/// Puts every device preference back to its default in one write.
pub(crate) fn restore_defaults(state: Store<UiModel>) {
    let mut change = serde_json::Map::new();
    for row in DEVICE.iter().flat_map(|rows| rows.iter()) {
        if let Ok(default) = serde_json::from_str::<Value>(row.default) {
            change.insert(row.key.to_owned(), default);
        }
    }
    patch(state, Value::Object(change));
}
