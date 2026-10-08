//! Client-visible configuration and keybinding payloads.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditorId {
    #[serde(rename = "cursor")]
    Cursor,
    #[serde(rename = "trae")]
    Trae,
    #[serde(rename = "kiro")]
    Kiro,
    #[serde(rename = "vscode")]
    Vscode,
    #[serde(rename = "vscode-insiders")]
    VscodeInsiders,
    #[serde(rename = "vscodium")]
    Vscodium,
    #[serde(rename = "zed")]
    Zed,
    #[serde(rename = "antigravity")]
    Antigravity,
    #[serde(rename = "idea")]
    Idea,
    #[serde(rename = "aqua")]
    Aqua,
    #[serde(rename = "clion")]
    Clion,
    #[serde(rename = "datagrip")]
    Datagrip,
    #[serde(rename = "dataspell")]
    Dataspell,
    #[serde(rename = "goland")]
    Goland,
    #[serde(rename = "phpstorm")]
    Phpstorm,
    #[serde(rename = "pycharm")]
    Pycharm,
    #[serde(rename = "rider")]
    Rider,
    #[serde(rename = "rubymine")]
    Rubymine,
    #[serde(rename = "rustrover")]
    Rustrover,
    #[serde(rename = "webstorm")]
    Webstorm,
    #[serde(rename = "file-manager")]
    FileManager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileManagerRevealKind {
    #[serde(rename = "finder")]
    Finder,
    #[serde(rename = "file-explorer")]
    FileExplorer,
    #[serde(rename = "files")]
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteOpenTargetKind {
    #[serde(rename = "tailscale")]
    Tailscale,
    #[serde(rename = "mdns")]
    Mdns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerDirectEndpointKind {
    #[serde(rename = "lan")]
    Lan,
    #[serde(rename = "tailnet")]
    Tailnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentThemeAppearance {
    #[serde(rename = "light")]
    Light,
    #[serde(rename = "dark")]
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteOpenTarget {
    pub kind: RemoteOpenTargetKind,

    pub host: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerDirectEndpoint {
    pub kind: ServerDirectEndpointKind,

    pub http_base_url: TrimmedNonEmptyString,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeybindingShortcut {
    pub key: BoundedTrimmedString<64>,

    pub meta_key: bool,

    pub ctrl_key: bool,

    pub shift_key: bool,

    pub alt_key: bool,

    pub mod_key: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeybindingRule {
    pub key: BoundedTrimmedString<64>,

    pub command: KeybindingCommand,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub when: Option<Option<BoundedTrimmedString<256>>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewForwardedShortcut {
    pub command: KeybindingCommand,

    pub shortcut: KeybindingShortcut,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedKeybindingRule {
    pub command: KeybindingCommand,

    pub shortcut: KeybindingShortcut,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub when_ast: Option<Option<KeybindingWhenNode>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentThemeVariants {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub light: Option<Option<EnvironmentThemeColors>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub dark: Option<Option<EnvironmentThemeColors>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentThemeFile {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub version: Option<Option<LiteralInt<1>>>,

    pub name: BoundedTrimmedString<48>,

    pub appearance: EnvironmentThemeAppearance,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub canvas: Option<Option<EnvironmentThemeColor>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub accent: Option<Option<EnvironmentThemeColor>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub colors: Option<Option<EnvironmentThemeColors>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub variants: Option<Option<EnvironmentThemeVariants>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvironmentTheme {
    pub id: EnvironmentThemeId,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub version: Option<Option<LiteralInt<1>>>,

    pub name: BoundedTrimmedString<48>,

    pub appearance: EnvironmentThemeAppearance,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub canvas: Option<Option<EnvironmentThemeColor>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub accent: Option<Option<EnvironmentThemeColor>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub colors: Option<Option<EnvironmentThemeColors>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub variants: Option<Option<EnvironmentThemeVariants>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitSourceAccount {
    pub id: TrimmedNonEmptyString,

    pub driver: ProviderDriverKind,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub email: Option<Option<TrimmedNonEmptyString>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub plan: Option<Option<TrimmedNonEmptyString>>,

    pub usage_limits: ServerProviderUsageLimits,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimitSourceSnapshot {
    pub id: String,

    pub kind: UsageLimitSourceKind,

    pub label: TrimmedNonEmptyString,

    pub checked_at: IsoDateTime,

    pub accounts: ForwardCompatibleArray<UsageLimitSourceAccount>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub error: Option<Option<TrimmedNonEmptyString>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub environment: ExecutionEnvironmentDescriptor,

    pub auth: ServerAuthDescriptor,

    pub cwd: TrimmedNonEmptyString,

    pub keybindings_config_path: TrimmedNonEmptyString,

    pub keybindings: ResolvedKeybindingsConfig,

    pub issues: ForwardCompatibleArray<ServerConfigIssue>,

    pub providers: ForwardCompatibleArray<ServerProvider>,

    pub available_editors: ForwardCompatibleArray<EditorId>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub remote_open_targets: Option<ForwardCompatibleArray<RemoteOpenTarget>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub direct_endpoints: Option<ForwardCompatibleArray<ServerDirectEndpoint>>,

    pub observability: ServerObservability,

    pub settings: ServerSettings,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub shell_resume_completion_marker: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub shell_reveal_in_file_manager: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_resume_completion_marker: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub thread_snapshot_pagination: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub reasoning_messages: Option<bool>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub shell_reveal_in_file_manager_kind: Option<FileManagerRevealKind>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub scratch_workspace_root: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub new_projects_root: Option<TrimmedNonEmptyString>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub environment_themes: Option<Option<Vec<EnvironmentTheme>>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub usage_limit_sources: Option<Option<ForwardCompatibleArray<UsageLimitSourceSnapshot>>>,
}

pub const STATIC_KEYBINDING_COMMANDS: &[&str] = &[
    "sidebar.toggle",
    "navigation.back",
    "navigation.forward",
    "terminal.toggle",
    "terminal.split",
    "terminal.splitVertical",
    "terminal.new",
    "terminal.close",
    "rightPanel.toggle",
    "rightPanel.new",
    "threadPanel.toggle",
    "rightPanel.toggleMaximized",
    "rightPanel.close",
    "view.reopenClosed",
    "pullRequest.copyNumber",
    "diff.toggle",
    "preview.toggle",
    "preview.refresh",
    "preview.focusUrl",
    "preview.zoomIn",
    "preview.zoomOut",
    "preview.resetZoom",
    "commandPalette.toggle",
    "filePicker.toggle",
    "projectSearch.toggle",
    "usage.open",
    "theme.select",
    "appearance.cycle",
    "themeEditor.toggle",
    "composer.stash",
    "composer.sendAlternate",
    "composer.sendBackground",
    "composer.sendAndNewThread",
    "composer.host",
    "composer.cycleHost",
    "composer.effort",
    "composer.mode",
    "composer.workspace",
    "composer.previousWorktree",
    "composer.branch",
    "chat.new",
    "chat.newLocal",
    "chat.newWithoutProject",
    "editor.openFavorite",
    "usage.cost",
    "usage.tokens",
    "usage.limits",
    "usage.period.day",
    "usage.period.week",
    "usage.period.month",
    "usage.period.quarter",
    "modelPicker.toggle",
    "modelPicker.previousProvider",
    "modelPicker.nextProvider",
    "modelPicker.jump.1",
    "modelPicker.jump.2",
    "modelPicker.jump.3",
    "modelPicker.jump.4",
    "modelPicker.jump.5",
    "modelPicker.jump.6",
    "modelPicker.jump.7",
    "modelPicker.jump.8",
    "modelPicker.jump.9",
    "thread.stop",
    "thread.steerQueuedMessage",
    "thread.editQueuedMessage",
    "thread.previous",
    "thread.next",
    "thread.copyReference",
    "thread.settle",
    "thread.pin",
    "thread.undo",
    "thread.jump.1",
    "thread.jump.2",
    "thread.jump.3",
    "thread.jump.4",
    "thread.jump.5",
    "thread.jump.6",
    "thread.jump.7",
    "thread.jump.8",
    "thread.jump.9",
];

pub type EnvironmentThemeColors = BTreeMap<EnvironmentThemeRole, BoundedTrimmedString<64>>;
fn theme_color(value: &str) -> Result<(), ValidationError> {
    if value.starts_with('#')
        && matches!(value.len(), 4 | 7)
        && value[1..].bytes().all(|c| c.is_ascii_hexdigit())
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a three- or six-digit hexadecimal theme color",
        })
    }
}
fn theme_id(value: &str) -> Result<(), ValidationError> {
    if !matches!(value, "system" | "light" | "dark")
        && !value.is_empty()
        && value.len() <= 48
        && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a non-reserved lowercase theme id of at most 48 characters",
        })
    }
}
fn theme_role(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphabetic()
        && value.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "an ASCII theme role name of at most 64 characters",
        })
    }
}
fn keybinding_command(value: &str) -> Result<(), ValidationError> {
    if STATIC_KEYBINDING_COMMANDS.contains(&value) {
        return Ok(());
    }
    if let Some(script) = value
        .strip_prefix("script.")
        .and_then(|v| v.strip_suffix(".run"))
    {
        if !script.is_empty()
            && script.len() <= 24
            && (script.as_bytes()[0].is_ascii_lowercase() || script.as_bytes()[0].is_ascii_digit())
            && script
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Ok(());
        }
    }
    Err(ValidationError {
        expected: "a declared keyboard command or valid script command",
    })
}
crate::base::plain_string_type!(EnvironmentThemeColor, theme_color);
crate::base::plain_string_type!(EnvironmentThemeId, theme_id);
crate::base::plain_string_type!(EnvironmentThemeRole, theme_role);
crate::base::plain_string_type!(KeybindingCommand, keybinding_command);
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum KeybindingWhenNode {
    Identifier {
        name: NonEmptyString,
    },
    Not {
        node: Box<KeybindingWhenNode>,
    },
    And {
        left: Box<KeybindingWhenNode>,
        right: Box<KeybindingWhenNode>,
    },
    Or {
        left: Box<KeybindingWhenNode>,
        right: Box<KeybindingWhenNode>,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ResolvedKeybindingsConfig(pub Vec<ResolvedKeybindingRule>);
impl<'de> Deserialize<'de> for ResolvedKeybindingsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let values = ForwardCompatibleArray::<ResolvedKeybindingRule>::deserialize(d)?.0;
        if values.len() <= 256 {
            Ok(Self(values))
        } else {
            Err(serde::de::Error::custom(
                "at most 256 keybindings are allowed",
            ))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ServerConfigIssue {
    #[serde(rename = "keybindings.malformed-config")]
    MalformedKeybindings { message: TrimmedNonEmptyString },
    #[serde(rename = "keybindings.invalid-entry")]
    InvalidKeybinding {
        message: TrimmedNonEmptyString,
        index: serde_json::Number,
    },
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct KeybindingsConfig(pub Vec<KeybindingRule>);
impl<'de> Deserialize<'de> for KeybindingsConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let values = Vec::<KeybindingRule>::deserialize(d)?;
        if values.len() <= 256 {
            Ok(Self(values))
        } else {
            Err(serde::de::Error::custom(
                "at most 256 keybindings are allowed",
            ))
        }
    }
}
pub fn environment_theme_file_has_colors(file: &EnvironmentThemeFile) -> bool {
    file.canvas.as_ref().is_some_and(Option::is_some)
        && file.accent.as_ref().is_some_and(Option::is_some)
        || file
            .colors
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|colors| !colors.is_empty())
}

/// Source-equivalent default shortcuts. The artifact is compiled into Rust;
/// loading server configuration requires no JavaScript runtime.
pub fn default_resolved_keybindings() -> ResolvedKeybindingsConfig {
    serde_json::from_str(include_str!("../assets/default-keybindings.json"))
        .expect("validated default keybindings")
}
