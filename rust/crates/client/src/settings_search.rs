//! Finding a setting by what it is called or what it does, from the Settings pages and from the
//! command palette. The catalog is the original's (`settingsSearch.ts`, copied by
//! `rust/tools/generate_settings_search_items.mjs`), and `searchSettings` ranks the same way. Only
//! entries whose setting exists in this app are offered (`placement`); the rest are listed with the
//! reason they are not, which is what the port's settings table reads.
use crate::palette::normalize_search_text;
use serde::Deserialize;
use std::sync::LazyLock;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: String,
    pub title: String,
    pub to: String,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub search_terms: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub desktop_only: bool,
    #[serde(default)]
    pub mac_only: bool,
    #[serde(default)]
    pub windows_only: bool,
    #[serde(default)]
    pub requires_thread_auto_settlement: bool,
    #[serde(default)]
    pub secondary: bool,
}

static ITEMS: LazyLock<Vec<Item>> = LazyLock::new(|| serde_json::from_str(include_str!("settings_search_items.json")).expect("the settings search catalog"));
pub fn items() -> &'static [Item] {
    &ITEMS
}

/// The section labels the results are subtitled with (`SETTINGS_SECTION_LABELS`).
pub fn section_label(to: &str) -> &'static str {
    match to {
        "projects" => "Project",
        "general" => "General",
        "appearance" => "Appearance",
        "keybindings" => "Keybindings",
        "snap-shot" => "SnapShots",
        "providers" => "Providers",
        "integrations" => "Integrations",
        "scheduled-tasks" => "Scheduled Tasks",
        "source-control" => "Source Control",
        "storage" => "Storage",
        "connections" => "Connections",
        "archived" => "Archive",
        _ => "Settings",
    }
}

/// What the running app is: which gates of the catalog hold.
#[derive(Debug, Clone, Copy, Default)]
pub struct Availability {
    pub desktop: bool,
    pub mac: bool,
    pub windows: bool,
    pub thread_auto_settlement: bool,
}

/// Where a catalog entry lives in this app, or why it does not yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A page of this app (by its key) and the anchor on it.
    Here(&'static str),
    /// The setting exists but its feature or a server method does not, said in the reason.
    Missing(&'static str),
}

/// Where each catalog entry lives in this app, or why it does not. The port's settings table.
pub fn placement(id: &str) -> Placement {
    match id {
        "storage-worktrees" => Placement::Here("general"),
        "storage-worktrees-location" => Placement::Here("general"),
        "storage-artifacts" => Placement::Here("general"),
        "project-defaults" => Placement::Here("projects"),
        "project-overview" => Placement::Here("projects"),
        "default-model" => Placement::Here("general"),
        "default-permissions" => Placement::Here("general"),
        "color-scheme" => Placement::Here("appearance"),
        "theme" => Placement::Here("appearance"),
        "setting-appearance-contrast" => Placement::Here("appearance"),
        "setting-glass-opacity" => Placement::Missing("unported feature: translucent menus and dialogs"),
        "diff-color-scheme" => Placement::Here("appearance"),
        "chat-width" => Placement::Here("appearance"),
        "panel-animations" => Placement::Here("appearance"),
        "environment-identification" => Placement::Missing("unported feature: the environment stage pill and artwork"),
        "interface-font" => Placement::Here("appearance"),
        "prompt-font" => Placement::Here("appearance"),
        "code-font" => Placement::Here("appearance"),
        "terminal-font" => Placement::Here("appearance"),
        "font-smoothing" => Placement::Here("appearance"),
        "word-wrap" => Placement::Here("appearance"),
        "composer-context" => Placement::Missing("unported feature: the composer's persistent context strip"),
        "project-grouping" => Placement::Missing("unported feature: grouped project rows in the sidebar"),
        "project-order" => Placement::Here("general"),
        "snooze-limited-threads" => Placement::Here("general"),
        "auto-resume-limited-threads" => Placement::Here("general"),
        "working-shelf" => Placement::Here("general"),
        "auto-settle-inactive-threads" => Placement::Here("general"),
        "auto-settle-merged-threads" => Placement::Here("general"),
        "days-before-auto-settle" => Placement::Here("general"),
        "thread-notifications" => Placement::Missing("unported feature: notifications when a thread finishes"),
        "in-app-notifications" => Placement::Missing("unported feature: in-app notifications for other threads"),
        "time-format" => Placement::Here("general"),
        "response-streaming" => Placement::Here("general"),
        "hide-whitespace-changes" => Placement::Here("general"),
        "default-diff-file-state" => Placement::Here("general"),
        "diff-layout" => Placement::Here("general"),
        "proactive-panels" => Placement::Missing("unported feature: panels that open on their own"),
        "skills-in-slash-menu" => Placement::Here("general"),
        "composer-rich-text" => Placement::Missing("unported feature: the rich text composer"),
        "composer-collapse" => Placement::Missing("unported feature: the composer's resting layout"),
        "send-shortcut" => Placement::Here("general"),
        "follow-up-behavior" => Placement::Here("general"),
        "provider-update-checks" => Placement::Here("general"),
        "continue-threads-after-server-update" => Placement::Here("general"),
        "background-activity" => Placement::Here("source-control"),
        "new-threads" => Placement::Here("general"),
        "worktree-submodules" => Placement::Here("general"),
        "start-from-origin" => Placement::Here("general"),
        "add-project-starts-in" => Placement::Here("general"),
        "unpin-confirmation" => Placement::Here("general"),
        "archive-confirmation" => Placement::Here("general"),
        "delete-confirmation" => Placement::Here("general"),
        "quit-confirmation" => Placement::Missing("unported feature: desktop quit handling"),
        "text-generation-model" => Placement::Here("text-generation"),
        "cli-command" => Placement::Missing("unported feature: the desktop t3 command installer"),
        "privacy-policy" => Placement::Missing("unported feature: privacy policy page"),
        "diagnostics" => Placement::Missing("unported feature: the Diagnostics page"),
        "open-source-licenses" => Placement::Missing("unported feature: the licenses page"),
        "legacy-plan-mode" => Placement::Here("general"),
        "legacy-context-window-indicator" => Placement::Here("general"),
        "legacy-sidebar" => Placement::Missing("unported feature: the legacy sidebar"),
        "keybindings" => Placement::Here("keybindings"),
        "snap-shot-enabled" => Placement::Missing("unported feature: SnapShots"),
        "snap-shot-accessibility" => Placement::Missing("unported feature: SnapShots"),
        "snap-shot-shortcut" => Placement::Missing("unported feature: SnapShots"),
        "snap-shot-sound" => Placement::Missing("unported feature: SnapShots"),
        "snap-shot-flash" => Placement::Missing("unported feature: SnapShots"),
        "snap-shot-animations" => Placement::Missing("unported feature: SnapShots"),
        "providers" => Placement::Here("providers"),
        "usage-providers" => Placement::Missing("unported feature: provider usage and health settings"),
        "cursor-keychain-usage" => Placement::Missing("unported feature: provider usage and health settings"),
        "provider-health-check-interval" => Placement::Missing("unported feature: provider usage and health settings"),
        "agent-browser-access" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "device-hosts" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "agent-device-access" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "device-hub" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "device-platform-support" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-profiles" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-default-profile" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-default-viewport" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-default-zoom" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-default-appearance" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-recording-frame-rate" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-recording-key-presses" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-recording-mouse-presses" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-link-target" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "browser-auto-show-floating-preview" => Placement::Missing("unported feature: the Integrations page (browser preview, device tools)"),
        "automatic-pull" => Placement::Here("general"),
        "remove-agent-credits-on-merge" => Placement::Here("general"),
        "pull-request-merge-method" => Placement::Here("general"),
        "source-control" => Placement::Here("source-control"),
        "git-fetch-interval" => Placement::Here("source-control"),
        "worktree-branch-naming" => Placement::Here("text-generation"),
        "github-accounts" => Placement::Here("source-control"),
        "bitbucket-credentials" => Placement::Here("source-control"),
        "source-control-writing-style" => Placement::Here("text-generation"),
        "follow-change-request-templates" => Placement::Here("text-generation"),
        "source-control-writer-model" => Placement::Here("text-generation"),
        "project-actions" => Placement::Missing("unported feature: project actions (scripts) editor"),
        "environment-icon" => Placement::Missing("unported feature: environment icons"),
        "local-environment" => Placement::Missing("unported feature: desktop and cloud connection management"),
        "network-access" => Placement::Missing("unported feature: desktop and cloud connection management"),
        "tailscale-https" => Placement::Missing("unported feature: desktop and cloud connection management"),
        "wsl-backend" => Placement::Missing("unported feature: desktop and cloud connection management"),
        "t3-connect" => Placement::Missing("unported feature: desktop and cloud connection management"),
        "hold-webhooks-while-offline" => Placement::Missing("unported feature: desktop and cloud connection management"),
        "publish-agent-activity" => Placement::Missing("unported feature: desktop and cloud connection management"),
        "connections-environment" => Placement::Here("connections"),
        "remote-environments" => Placement::Here("connections"),
        "load-balancing" => Placement::Missing("unported feature: load balancing between environments"),
        "github-routing" => Placement::Missing("unported feature: GitHub sharing between environments"),
        "archive" => Placement::Missing("shown in the sidebar's Archived list, not a settings page"),
        _ => Placement::Missing("unported feature"),
    }
}

/// `searchSettings`: every entry the gates allow whose words contain every word typed, best first.
pub fn search<'a>(query: &str, items: impl IntoIterator<Item = &'a Item>, availability: Availability) -> Vec<&'a Item> {
    let normalized = normalize_search_text(query);
    if normalized.is_empty() {
        return vec![];
    }
    let tokens: Vec<&str> = normalized.split(' ').collect();
    let mut found: Vec<(&Item, usize, u8)> = items
        .into_iter()
        .enumerate()
        .filter_map(|(index, item)| {
            if (item.desktop_only && !availability.desktop) || (item.mac_only && !availability.mac) || (item.windows_only && !availability.windows) || (item.requires_thread_auto_settlement && !availability.thread_auto_settlement) {
                return None;
            }
            let title = normalize_search_text(&item.title);
            let mut fields = vec![title.clone(), normalize_search_text(section_label(&item.to))];
            fields.extend(item.search_terms.iter().map(|term| normalize_search_text(term)));
            if !tokens.iter().all(|token| fields.iter().any(|field| field.contains(token))) {
                return None;
            }
            let exact_phrase = fields.iter().any(|field| field.contains(&normalized));
            let rank = if title == normalized {
                5
            } else if title.starts_with(&normalized) {
                4
            } else if title.contains(&normalized) {
                3
            } else if tokens.iter().all(|token| title.contains(token)) {
                2
            } else if exact_phrase {
                1
            } else {
                0
            };
            Some((item, index, rank))
        })
        .collect();
    found.sort_by(|left, right| left.0.secondary.cmp(&right.0.secondary).then(right.2.cmp(&left.2)).then(left.1.cmp(&right.1)));
    found.into_iter().map(|(item, ..)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_is_the_originals_and_ranks_titles_before_words_in_descriptions() {
        assert!(items().len() > 100);
        let all = Availability { desktop: true, mac: true, windows: true, thread_auto_settlement: true };
        let hits = search("model", items(), all);
        assert_eq!(hits.first().map(|item| item.title.as_str()), Some("Default model"), "{:?}", hits.iter().take(4).map(|item| &item.title).collect::<Vec<_>>());
        assert!(search("   ", items(), all).is_empty());
        // Every word has to be found somewhere in the entry.
        assert!(search("worktree zzzzqq", items(), all).is_empty());
        assert!(search("worktree location", items(), all).iter().any(|item| item.id == "storage-worktrees-location"));
    }

    #[test]
    fn gates_hide_what_this_machine_cannot_show() {
        let browser = Availability::default();
        assert!(search("font smoothing", items(), browser).is_empty(), "macOS only");
        assert!(search("font smoothing", items(), Availability { mac: true, ..Default::default() }).iter().any(|item| item.id == "font-smoothing"));
        assert!(search("quit", items(), browser).iter().all(|item| item.id != "quit-confirmation"), "desktop only");
        assert!(search("days of inactivity", items(), browser).is_empty(), "needs the server to settle threads");
    }
}
