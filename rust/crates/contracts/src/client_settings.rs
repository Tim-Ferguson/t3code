//! Client-local settings and canonical persisted preferences.
//! Defaults and transforms follow packages/contracts/src/settings.ts.
use crate::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationMode {
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "notifications")]
    Notifications,
    #[serde(rename = "sound")]
    Sound,
    #[serde(rename = "notifications-and-sound")]
    NotificationsAndSound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffColorScheme {
    #[serde(rename = "red-green")]
    RedGreen,
    #[serde(rename = "blue-orange")]
    BlueOrange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatWidth {
    #[serde(rename = "comfortable")]
    Comfortable,
    #[serde(rename = "wide")]
    Wide,
    #[serde(rename = "full")]
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreviewAppearancePreference {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "light")]
    Light,
    #[serde(rename = "dark")]
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserLinkTarget {
    #[serde(rename = "system")]
    System,
    #[serde(rename = "app")]
    App,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLayout {
    #[serde(rename = "stacked")]
    Stacked,
    #[serde(rename = "split")]
    Split,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvironmentIdentificationMode {
    #[serde(rename = "artwork")]
    Artwork,
    #[serde(rename = "pill")]
    Pill,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendShortcut {
    #[serde(rename = "enter")]
    Enter,
    #[serde(rename = "mod-enter-multiline")]
    ModEnterMultiline,
    #[serde(rename = "mod-enter")]
    ModEnter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FollowUpBehavior {
    #[serde(rename = "queue")]
    Queue,
    #[serde(rename = "steer")]
    Steer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarProjectGroupingMode {
    #[serde(rename = "repository")]
    Repository,
    #[serde(rename = "repository_path")]
    RepositoryPath,
    #[serde(rename = "separate")]
    Separate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarProjectSortOrder {
    #[serde(rename = "updated_at")]
    UpdatedAt,
    #[serde(rename = "created_at")]
    CreatedAt,
    #[serde(rename = "manual")]
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarThreadSortOrder {
    #[serde(rename = "updated_at")]
    UpdatedAt,
    #[serde(rename = "created_at")]
    CreatedAt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimestampFormat {
    #[serde(rename = "locale")]
    Locale,
    #[serde(rename = "12-hour")]
    Hour12Hour,
    #[serde(rename = "24-hour")]
    Hour24Hour,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapShotSound {
    #[serde(rename = "soft-pop")]
    SoftPop,
    #[serde(rename = "camera-shutter")]
    CameraShutter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapShotModifier {
    #[serde(rename = "shift")]
    Shift,
    #[serde(rename = "meta")]
    Meta,
    #[serde(rename = "control")]
    Control,
    #[serde(rename = "alt")]
    Alt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserProfileKind {
    #[serde(rename = "persistent")]
    Persistent,
    #[serde(rename = "incognito")]
    Incognito,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StoredPreviewViewportPresetId {
    #[serde(rename = "iphone-se")]
    IphoneSe,
    #[serde(rename = "iphone-xr")]
    IphoneXr,
    #[serde(rename = "iphone-12-pro")]
    Iphone12Pro,
    #[serde(rename = "iphone-14-pro-max")]
    Iphone14ProMax,
    #[serde(rename = "pixel-7")]
    Pixel7,
    #[serde(rename = "samsung-galaxy-s8-plus")]
    SamsungGalaxyS8Plus,
    #[serde(rename = "samsung-galaxy-s20-ultra")]
    SamsungGalaxyS20Ultra,
    #[serde(rename = "ipad-mini")]
    IpadMini,
    #[serde(rename = "ipad-air")]
    IpadAir,
    #[serde(rename = "ipad-pro")]
    IpadPro,
    #[serde(rename = "surface-pro-7")]
    SurfacePro7,
    #[serde(rename = "surface-duo")]
    SurfaceDuo,
    #[serde(rename = "galaxy-z-fold-5")]
    GalaxyZFold5,
    #[serde(rename = "asus-zenbook-fold")]
    AsusZenbookFold,
    #[serde(rename = "samsung-galaxy-a51-71")]
    SamsungGalaxyA5171,
    #[serde(rename = "nest-hub")]
    NestHub,
    #[serde(rename = "nest-hub-max")]
    NestHubMax,
    #[serde(rename = "desktop-1920x1080")]
    Desktop1920x1080,
    #[serde(rename = "desktop-1440x900")]
    Desktop1440x900,
    #[serde(rename = "laptop-1366x768")]
    Laptop1366x768,
    #[serde(rename = "laptop-1280x800")]
    Laptop1280x800,
    #[serde(rename = "ipad-pro-11")]
    IpadPro11,
    #[serde(rename = "iphone-15-pro")]
    Iphone15Pro,
    #[serde(rename = "pixel-8")]
    Pixel8,
    #[serde(rename = "galaxy-s24")]
    GalaxyS24,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuitConfirmationMode {
    #[serde(rename = "direct")]
    Direct,
    #[serde(rename = "hold")]
    Hold,
    #[serde(rename = "double-click")]
    DoubleClick,
}

pub type AppearanceContrast = RangeInt<50, 200>;
pub type PanelAnimationDurationMs = RangeInt<0, 400>;
pub type GlassOpacity = RangeInt<40, 100>;
pub type InterfaceFontSize = RangeInt<12, 20>;
pub type PromptFontSize = RangeInt<12, 20>;
pub type CodeFontSize = RangeInt<10, 18>;
pub type TerminalFontSize = RangeInt<8, 20>;
pub type SidebarThreadPreviewCount = RangeInt<1, 15>;
pub type FontFamilyPreference = BoundedString<200>;
pub type LoadBalancingWeights = BTreeMap<TrimmedNonEmptyString, RangeInt<0, 100>>;

macro_rules! defaulted {
    ($name:ident, $ty:ty, $json:literal) => {
        mod $name {
            use super::*;
            pub fn default() -> $ty {
                serde_json::from_str($json).expect("source-backed client settings default")
            }
            pub fn decode<'de, D: serde::Deserializer<'de>>(d: D) -> Result<$ty, D::Error> {
                let value = serde_json::Value::deserialize(d)?;
                if value.is_null() {
                    Ok(default())
                } else {
                    serde_json::from_value(value).map_err(serde::de::Error::custom)
                }
            }
        }
    };
}

defaulted!(notification_mode, NotificationMode, "\"off\"");
defaulted!(in_app_notifications_enabled, bool, "false");
defaulted!(diff_color_scheme, DiffColorScheme, "\"red-green\"");
defaulted!(chat_width, ChatWidth, "\"comfortable\"");
defaulted!(load_balancing_enabled, bool, "false");
defaulted!(load_balancing_weights, LoadBalancingWeights, "{}");
defaulted!(appearance_contrast, AppearanceContrast, "100");
defaulted!(panel_animation_duration_ms, PanelAnimationDurationMs, "0");
defaulted!(
    browser_default_viewport,
    PreviewViewportSetting,
    "{\"_tag\":\"fill\"}"
);
defaulted!(browser_default_zoom_factor, PreviewZoomFactor, "1");
defaulted!(
    browser_default_appearance,
    PreviewAppearancePreference,
    "\"system\""
);
defaulted!(
    browser_recording_frame_rate,
    BrowserRecordingFrameRate,
    "30"
);
defaulted!(browser_recording_show_key_presses, bool, "false");
defaulted!(browser_recording_show_mouse_presses, bool, "false");
defaulted!(browser_link_target, BrowserLinkTarget, "\"system\"");
defaulted!(browser_auto_show_floating_preview, bool, "true");
defaulted!(browser_profiles, Vec<BrowserProfile>, "[]");
defaulted!(browser_default_profile_id, BrowserProfileId, "\"default\"");
defaulted!(confirm_quit, QuitConfirmationMode, "\"hold\"");
defaulted!(confirm_thread_archive, bool, "false");
defaulted!(confirm_thread_delete, bool, "true");
defaulted!(confirm_thread_unpin, bool, "false");
defaulted!(
    dismissed_provider_update_notification_keys,
    Vec<TrimmedNonEmptyString>,
    "[]"
);
defaulted!(diff_files_collapsed, bool, "true");
defaulted!(diff_ignore_whitespace, bool, "true");
defaulted!(diff_layout, DiffLayout, "\"stacked\"");
defaulted!(
    environment_identification_mode,
    EnvironmentIdentificationMode,
    "\"artwork\""
);
defaulted!(glass_opacity, GlassOpacity, "80");
defaulted!(font_size_interface, InterfaceFontSize, "16");
defaulted!(font_size_prompt, PromptFontSize, "14");
defaulted!(font_size_code, CodeFontSize, "13");
defaulted!(font_size_terminal, TerminalFontSize, "12");
defaulted!(font_family_code, FontFamilyPreference, "\"\"");
defaulted!(font_family_composer, FontFamilyPreference, "\"\"");
defaulted!(font_family_sans, FontFamilyPreference, "\"\"");
defaulted!(font_family_terminal, FontFamilyPreference, "\"\"");
defaulted!(font_smoothing, bool, "true");
defaulted!(onboarding_completed_at, Option<String>, "null");
defaulted!(persist_composer_context_strip, bool, "false");
defaulted!(favorites, Vec<ModelFavorite>, "[]");
defaulted!(provider_model_preferences, BTreeMap<ProviderInstanceId, ProviderModelPreferences>, "{}");
defaulted!(pull_request_merge_method_overrides, BTreeMap<TrimmedNonEmptyString, PullRequestMergeMethod>, "{}");
defaulted!(plan_mode_enabled, bool, "false");
defaulted!(context_window_meter_enabled, bool, "false");
defaulted!(composer_collapse_on_scroll, bool, "true");
defaulted!(composer_rich_text_enabled, bool, "true");
defaulted!(send_shortcut, SendShortcut, "\"enter\"");
defaulted!(follow_up_behavior, FollowUpBehavior, "\"queue\"");
defaulted!(proactive_panels_enabled, bool, "false");
defaulted!(show_skills_in_slash_menu, bool, "true");
defaulted!(legacy_sidebar_enabled, bool, "false");
defaulted!(sidebar_working_shelf_enabled, bool, "false");
defaulted!(
    sidebar_project_grouping_mode,
    SidebarProjectGroupingMode,
    "\"repository\""
);
defaulted!(sidebar_project_grouping_overrides, BTreeMap<TrimmedNonEmptyString, SidebarProjectGroupingMode>, "{}");
defaulted!(
    sidebar_project_sort_order,
    SidebarProjectSortOrder,
    "\"updated_at\""
);
defaulted!(
    sidebar_thread_sort_order,
    SidebarThreadSortOrder,
    "\"updated_at\""
);
defaulted!(sidebar_thread_preview_count, SidebarThreadPreviewCount, "6");
defaulted!(timestamp_format, TimestampFormat, "\"locale\"");
defaulted!(snap_shot_enabled, bool, "false");
defaulted!(snap_shot_include_accessibility, bool, "true");
defaulted!(
    snap_shot_shortcut,
    SnapShotShortcut,
    "{\"kind\":\"both-shift-keys\"}"
);
defaulted!(snap_shot_play_sound, bool, "true");
defaulted!(snap_shot_sound, SnapShotSound, "\"soft-pop\"");
defaulted!(snap_shot_flash, bool, "true");
defaulted!(snap_shot_animations, bool, "true");
defaulted!(word_wrap, bool, "true");
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "BTreeMap<String, serde_json::Value>")]
pub struct ClientSettings {
    #[serde(
        default = "notification_mode::default",
        deserialize_with = "notification_mode::decode"
    )]
    pub notification_mode: NotificationMode,
    #[serde(
        default = "in_app_notifications_enabled::default",
        deserialize_with = "in_app_notifications_enabled::decode"
    )]
    pub in_app_notifications_enabled: bool,
    #[serde(
        default = "diff_color_scheme::default",
        deserialize_with = "diff_color_scheme::decode"
    )]
    pub diff_color_scheme: DiffColorScheme,
    #[serde(
        default = "chat_width::default",
        deserialize_with = "chat_width::decode"
    )]
    pub chat_width: ChatWidth,
    #[serde(
        default = "load_balancing_enabled::default",
        deserialize_with = "load_balancing_enabled::decode"
    )]
    pub load_balancing_enabled: bool,
    #[serde(
        default = "load_balancing_weights::default",
        deserialize_with = "load_balancing_weights::decode"
    )]
    pub load_balancing_weights: LoadBalancingWeights,
    #[serde(
        default = "appearance_contrast::default",
        deserialize_with = "appearance_contrast::decode"
    )]
    pub appearance_contrast: AppearanceContrast,
    #[serde(
        default = "panel_animation_duration_ms::default",
        deserialize_with = "panel_animation_duration_ms::decode"
    )]
    pub panel_animation_duration_ms: PanelAnimationDurationMs,
    #[serde(
        default = "browser_default_viewport::default",
        deserialize_with = "browser_default_viewport::decode"
    )]
    pub browser_default_viewport: PreviewViewportSetting,
    #[serde(
        default = "browser_default_zoom_factor::default",
        deserialize_with = "browser_default_zoom_factor::decode"
    )]
    pub browser_default_zoom_factor: PreviewZoomFactor,
    #[serde(
        default = "browser_default_appearance::default",
        deserialize_with = "browser_default_appearance::decode"
    )]
    pub browser_default_appearance: PreviewAppearancePreference,
    #[serde(
        default = "browser_recording_frame_rate::default",
        deserialize_with = "browser_recording_frame_rate::decode"
    )]
    pub browser_recording_frame_rate: BrowserRecordingFrameRate,
    #[serde(
        default = "browser_recording_show_key_presses::default",
        deserialize_with = "browser_recording_show_key_presses::decode"
    )]
    pub browser_recording_show_key_presses: bool,
    #[serde(
        default = "browser_recording_show_mouse_presses::default",
        deserialize_with = "browser_recording_show_mouse_presses::decode"
    )]
    pub browser_recording_show_mouse_presses: bool,
    #[serde(
        default = "browser_link_target::default",
        deserialize_with = "browser_link_target::decode"
    )]
    pub browser_link_target: BrowserLinkTarget,
    #[serde(
        default = "browser_auto_show_floating_preview::default",
        deserialize_with = "browser_auto_show_floating_preview::decode"
    )]
    pub browser_auto_show_floating_preview: bool,
    #[serde(
        default = "browser_profiles::default",
        deserialize_with = "browser_profiles::decode"
    )]
    pub browser_profiles: Vec<BrowserProfile>,
    #[serde(
        default = "browser_default_profile_id::default",
        deserialize_with = "browser_default_profile_id::decode"
    )]
    pub browser_default_profile_id: BrowserProfileId,
    #[serde(
        default = "confirm_quit::default",
        deserialize_with = "decode_quit_confirmation_setting"
    )]
    pub confirm_quit: QuitConfirmationMode,
    #[serde(
        default = "confirm_thread_archive::default",
        deserialize_with = "confirm_thread_archive::decode"
    )]
    pub confirm_thread_archive: bool,
    #[serde(
        default = "confirm_thread_delete::default",
        deserialize_with = "confirm_thread_delete::decode"
    )]
    pub confirm_thread_delete: bool,
    #[serde(
        default = "confirm_thread_unpin::default",
        deserialize_with = "confirm_thread_unpin::decode"
    )]
    pub confirm_thread_unpin: bool,
    #[serde(
        default = "dismissed_provider_update_notification_keys::default",
        deserialize_with = "dismissed_provider_update_notification_keys::decode"
    )]
    pub dismissed_provider_update_notification_keys: Vec<TrimmedNonEmptyString>,
    #[serde(
        default = "diff_files_collapsed::default",
        deserialize_with = "diff_files_collapsed::decode"
    )]
    pub diff_files_collapsed: bool,
    #[serde(
        default = "diff_ignore_whitespace::default",
        deserialize_with = "diff_ignore_whitespace::decode"
    )]
    pub diff_ignore_whitespace: bool,
    #[serde(
        default = "diff_layout::default",
        deserialize_with = "diff_layout::decode"
    )]
    pub diff_layout: DiffLayout,
    #[serde(
        default = "environment_identification_mode::default",
        deserialize_with = "environment_identification_mode::decode"
    )]
    pub environment_identification_mode: EnvironmentIdentificationMode,
    #[serde(
        default = "glass_opacity::default",
        deserialize_with = "glass_opacity::decode"
    )]
    pub glass_opacity: GlassOpacity,
    #[serde(
        default = "font_size_interface::default",
        deserialize_with = "font_size_interface::decode"
    )]
    pub font_size_interface: InterfaceFontSize,
    #[serde(
        default = "font_size_prompt::default",
        deserialize_with = "font_size_prompt::decode"
    )]
    pub font_size_prompt: PromptFontSize,
    #[serde(
        default = "font_size_code::default",
        deserialize_with = "font_size_code::decode"
    )]
    pub font_size_code: CodeFontSize,
    #[serde(
        default = "font_size_terminal::default",
        deserialize_with = "font_size_terminal::decode"
    )]
    pub font_size_terminal: TerminalFontSize,
    #[serde(
        default = "font_family_code::default",
        deserialize_with = "font_family_code::decode"
    )]
    pub font_family_code: FontFamilyPreference,
    #[serde(
        default = "font_family_composer::default",
        deserialize_with = "font_family_composer::decode"
    )]
    pub font_family_composer: FontFamilyPreference,
    #[serde(
        default = "font_family_sans::default",
        deserialize_with = "font_family_sans::decode"
    )]
    pub font_family_sans: FontFamilyPreference,
    #[serde(
        default = "font_family_terminal::default",
        deserialize_with = "font_family_terminal::decode"
    )]
    pub font_family_terminal: FontFamilyPreference,
    #[serde(
        default = "font_smoothing::default",
        deserialize_with = "font_smoothing::decode"
    )]
    pub font_smoothing: bool,
    #[serde(
        default = "onboarding_completed_at::default",
        deserialize_with = "onboarding_completed_at::decode"
    )]
    pub onboarding_completed_at: Option<String>,
    #[serde(
        default = "persist_composer_context_strip::default",
        deserialize_with = "persist_composer_context_strip::decode"
    )]
    pub persist_composer_context_strip: bool,
    #[serde(default = "favorites::default", deserialize_with = "favorites::decode")]
    pub favorites: Vec<ModelFavorite>,
    #[serde(
        default = "provider_model_preferences::default",
        deserialize_with = "provider_model_preferences::decode"
    )]
    pub provider_model_preferences: BTreeMap<ProviderInstanceId, ProviderModelPreferences>,
    #[serde(
        default = "pull_request_merge_method_overrides::default",
        deserialize_with = "pull_request_merge_method_overrides::decode"
    )]
    pub pull_request_merge_method_overrides:
        BTreeMap<TrimmedNonEmptyString, PullRequestMergeMethod>,
    #[serde(
        default = "plan_mode_enabled::default",
        deserialize_with = "plan_mode_enabled::decode"
    )]
    pub plan_mode_enabled: bool,
    #[serde(
        default = "context_window_meter_enabled::default",
        deserialize_with = "context_window_meter_enabled::decode"
    )]
    pub context_window_meter_enabled: bool,
    #[serde(
        default = "composer_collapse_on_scroll::default",
        deserialize_with = "composer_collapse_on_scroll::decode"
    )]
    pub composer_collapse_on_scroll: bool,
    #[serde(
        default = "composer_rich_text_enabled::default",
        deserialize_with = "composer_rich_text_enabled::decode"
    )]
    pub composer_rich_text_enabled: bool,
    #[serde(
        default = "send_shortcut::default",
        deserialize_with = "send_shortcut::decode"
    )]
    pub send_shortcut: SendShortcut,
    #[serde(
        default = "follow_up_behavior::default",
        deserialize_with = "follow_up_behavior::decode"
    )]
    pub follow_up_behavior: FollowUpBehavior,
    #[serde(
        default = "proactive_panels_enabled::default",
        deserialize_with = "proactive_panels_enabled::decode"
    )]
    pub proactive_panels_enabled: bool,
    #[serde(
        default = "show_skills_in_slash_menu::default",
        deserialize_with = "show_skills_in_slash_menu::decode"
    )]
    pub show_skills_in_slash_menu: bool,
    #[serde(
        default = "legacy_sidebar_enabled::default",
        deserialize_with = "legacy_sidebar_enabled::decode"
    )]
    pub legacy_sidebar_enabled: bool,
    #[serde(
        default = "sidebar_working_shelf_enabled::default",
        deserialize_with = "sidebar_working_shelf_enabled::decode"
    )]
    pub sidebar_working_shelf_enabled: bool,
    #[serde(
        default = "sidebar_project_grouping_mode::default",
        deserialize_with = "sidebar_project_grouping_mode::decode"
    )]
    pub sidebar_project_grouping_mode: SidebarProjectGroupingMode,
    #[serde(
        default = "sidebar_project_grouping_overrides::default",
        deserialize_with = "sidebar_project_grouping_overrides::decode"
    )]
    pub sidebar_project_grouping_overrides:
        BTreeMap<TrimmedNonEmptyString, SidebarProjectGroupingMode>,
    #[serde(
        default = "sidebar_project_sort_order::default",
        deserialize_with = "sidebar_project_sort_order::decode"
    )]
    pub sidebar_project_sort_order: SidebarProjectSortOrder,
    #[serde(
        default = "sidebar_thread_sort_order::default",
        deserialize_with = "sidebar_thread_sort_order::decode"
    )]
    pub sidebar_thread_sort_order: SidebarThreadSortOrder,
    #[serde(
        default = "sidebar_thread_preview_count::default",
        deserialize_with = "sidebar_thread_preview_count::decode"
    )]
    pub sidebar_thread_preview_count: SidebarThreadPreviewCount,
    #[serde(
        default = "timestamp_format::default",
        deserialize_with = "timestamp_format::decode"
    )]
    pub timestamp_format: TimestampFormat,
    #[serde(
        default = "snap_shot_enabled::default",
        deserialize_with = "snap_shot_enabled::decode"
    )]
    pub snap_shot_enabled: bool,
    #[serde(
        default = "snap_shot_include_accessibility::default",
        deserialize_with = "snap_shot_include_accessibility::decode"
    )]
    pub snap_shot_include_accessibility: bool,
    #[serde(
        default = "snap_shot_shortcut::default",
        deserialize_with = "snap_shot_shortcut::decode"
    )]
    pub snap_shot_shortcut: SnapShotShortcut,
    #[serde(
        default = "snap_shot_play_sound::default",
        deserialize_with = "snap_shot_play_sound::decode"
    )]
    pub snap_shot_play_sound: bool,
    #[serde(
        default = "snap_shot_sound::default",
        deserialize_with = "snap_shot_sound::decode"
    )]
    pub snap_shot_sound: SnapShotSound,
    #[serde(
        default = "snap_shot_flash::default",
        deserialize_with = "snap_shot_flash::decode"
    )]
    pub snap_shot_flash: bool,
    #[serde(
        default = "snap_shot_animations::default",
        deserialize_with = "snap_shot_animations::decode"
    )]
    pub snap_shot_animations: bool,
    #[serde(default = "word_wrap::default", deserialize_with = "word_wrap::decode")]
    pub word_wrap: bool,
}
impl TryFrom<BTreeMap<String, serde_json::Value>> for ClientSettings {
    type Error = serde_json::Error;
    fn try_from(mut value: BTreeMap<String, serde_json::Value>) -> Result<Self, Self::Error> {
        Ok(Self {
            pull_request_merge_method_overrides: match value
                .remove("pullRequestMergeMethodOverrides")
            {
                Some(value) => pull_request_merge_method_overrides::decode(value)?,
                None => pull_request_merge_method_overrides::default(),
            },
            sidebar_project_grouping_overrides: match value
                .remove("sidebarProjectGroupingOverrides")
            {
                Some(value) => sidebar_project_grouping_overrides::decode(value)?,
                None => sidebar_project_grouping_overrides::default(),
            },
            notification_mode: match value.remove("notificationMode") {
                Some(value) => notification_mode::decode(value)?,
                None => notification_mode::default(),
            },
            in_app_notifications_enabled: match value.remove("inAppNotificationsEnabled") {
                Some(value) => in_app_notifications_enabled::decode(value)?,
                None => in_app_notifications_enabled::default(),
            },
            diff_color_scheme: match value.remove("diffColorScheme") {
                Some(value) => diff_color_scheme::decode(value)?,
                None => diff_color_scheme::default(),
            },
            chat_width: match value.remove("chatWidth") {
                Some(value) => chat_width::decode(value)?,
                None => chat_width::default(),
            },
            load_balancing_enabled: match value.remove("loadBalancingEnabled") {
                Some(value) => load_balancing_enabled::decode(value)?,
                None => load_balancing_enabled::default(),
            },
            load_balancing_weights: match value.remove("loadBalancingWeights") {
                Some(value) => load_balancing_weights::decode(value)?,
                None => load_balancing_weights::default(),
            },
            appearance_contrast: match value.remove("appearanceContrast") {
                Some(value) => appearance_contrast::decode(value)?,
                None => appearance_contrast::default(),
            },
            panel_animation_duration_ms: match value.remove("panelAnimationDurationMs") {
                Some(value) => panel_animation_duration_ms::decode(value)?,
                None => panel_animation_duration_ms::default(),
            },
            browser_default_viewport: match value.remove("browserDefaultViewport") {
                Some(value) => browser_default_viewport::decode(value)?,
                None => browser_default_viewport::default(),
            },
            browser_default_zoom_factor: match value.remove("browserDefaultZoomFactor") {
                Some(value) => browser_default_zoom_factor::decode(value)?,
                None => browser_default_zoom_factor::default(),
            },
            browser_default_appearance: match value.remove("browserDefaultAppearance") {
                Some(value) => browser_default_appearance::decode(value)?,
                None => browser_default_appearance::default(),
            },
            browser_recording_frame_rate: match value.remove("browserRecordingFrameRate") {
                Some(value) => browser_recording_frame_rate::decode(value)?,
                None => browser_recording_frame_rate::default(),
            },
            browser_recording_show_key_presses: match value.remove("browserRecordingShowKeyPresses")
            {
                Some(value) => browser_recording_show_key_presses::decode(value)?,
                None => browser_recording_show_key_presses::default(),
            },
            browser_recording_show_mouse_presses: match value
                .remove("browserRecordingShowMousePresses")
            {
                Some(value) => browser_recording_show_mouse_presses::decode(value)?,
                None => browser_recording_show_mouse_presses::default(),
            },
            browser_link_target: match value.remove("browserLinkTarget") {
                Some(value) => browser_link_target::decode(value)?,
                None => browser_link_target::default(),
            },
            browser_auto_show_floating_preview: match value.remove("browserAutoShowFloatingPreview")
            {
                Some(value) => browser_auto_show_floating_preview::decode(value)?,
                None => browser_auto_show_floating_preview::default(),
            },
            browser_profiles: match value.remove("browserProfiles") {
                Some(value) => browser_profiles::decode(value)?,
                None => browser_profiles::default(),
            },
            browser_default_profile_id: match value.remove("browserDefaultProfileId") {
                Some(value) => browser_default_profile_id::decode(value)?,
                None => browser_default_profile_id::default(),
            },
            confirm_quit: match value.remove("confirmQuit") {
                Some(value) => decode_quit_confirmation_setting(value)?,
                None => confirm_quit::default(),
            },
            confirm_thread_archive: match value.remove("confirmThreadArchive") {
                Some(value) => confirm_thread_archive::decode(value)?,
                None => confirm_thread_archive::default(),
            },
            confirm_thread_delete: match value.remove("confirmThreadDelete") {
                Some(value) => confirm_thread_delete::decode(value)?,
                None => confirm_thread_delete::default(),
            },
            confirm_thread_unpin: match value.remove("confirmThreadUnpin") {
                Some(value) => confirm_thread_unpin::decode(value)?,
                None => confirm_thread_unpin::default(),
            },
            dismissed_provider_update_notification_keys: match value
                .remove("dismissedProviderUpdateNotificationKeys")
            {
                Some(value) => dismissed_provider_update_notification_keys::decode(value)?,
                None => dismissed_provider_update_notification_keys::default(),
            },
            diff_files_collapsed: match value.remove("diffFilesCollapsed") {
                Some(value) => diff_files_collapsed::decode(value)?,
                None => diff_files_collapsed::default(),
            },
            diff_ignore_whitespace: match value.remove("diffIgnoreWhitespace") {
                Some(value) => diff_ignore_whitespace::decode(value)?,
                None => diff_ignore_whitespace::default(),
            },
            diff_layout: match value.remove("diffLayout") {
                Some(value) => diff_layout::decode(value)?,
                None => diff_layout::default(),
            },
            environment_identification_mode: match value.remove("environmentIdentificationMode") {
                Some(value) => environment_identification_mode::decode(value)?,
                None => environment_identification_mode::default(),
            },
            glass_opacity: match value.remove("glassOpacity") {
                Some(value) => glass_opacity::decode(value)?,
                None => glass_opacity::default(),
            },
            font_size_interface: match value.remove("fontSizeInterface") {
                Some(value) => font_size_interface::decode(value)?,
                None => font_size_interface::default(),
            },
            font_size_prompt: match value.remove("fontSizePrompt") {
                Some(value) => font_size_prompt::decode(value)?,
                None => font_size_prompt::default(),
            },
            font_size_code: match value.remove("fontSizeCode") {
                Some(value) => font_size_code::decode(value)?,
                None => font_size_code::default(),
            },
            font_size_terminal: match value.remove("fontSizeTerminal") {
                Some(value) => font_size_terminal::decode(value)?,
                None => font_size_terminal::default(),
            },
            font_family_code: match value.remove("fontFamilyCode") {
                Some(value) => font_family_code::decode(value)?,
                None => font_family_code::default(),
            },
            font_family_composer: match value.remove("fontFamilyComposer") {
                Some(value) => font_family_composer::decode(value)?,
                None => font_family_composer::default(),
            },
            font_family_sans: match value.remove("fontFamilySans") {
                Some(value) => font_family_sans::decode(value)?,
                None => font_family_sans::default(),
            },
            font_family_terminal: match value.remove("fontFamilyTerminal") {
                Some(value) => font_family_terminal::decode(value)?,
                None => font_family_terminal::default(),
            },
            font_smoothing: match value.remove("fontSmoothing") {
                Some(value) => font_smoothing::decode(value)?,
                None => font_smoothing::default(),
            },
            onboarding_completed_at: match value.remove("onboardingCompletedAt") {
                Some(value) => onboarding_completed_at::decode(value)?,
                None => onboarding_completed_at::default(),
            },
            persist_composer_context_strip: match value.remove("persistComposerContextStrip") {
                Some(value) => persist_composer_context_strip::decode(value)?,
                None => persist_composer_context_strip::default(),
            },
            favorites: match value.remove("favorites") {
                Some(value) => favorites::decode(value)?,
                None => favorites::default(),
            },
            provider_model_preferences: match value.remove("providerModelPreferences") {
                Some(value) => provider_model_preferences::decode(value)?,
                None => provider_model_preferences::default(),
            },
            plan_mode_enabled: match value.remove("planModeEnabled") {
                Some(value) => plan_mode_enabled::decode(value)?,
                None => plan_mode_enabled::default(),
            },
            context_window_meter_enabled: match value.remove("contextWindowMeterEnabled") {
                Some(value) => context_window_meter_enabled::decode(value)?,
                None => context_window_meter_enabled::default(),
            },
            composer_collapse_on_scroll: match value.remove("composerCollapseOnScroll") {
                Some(value) => composer_collapse_on_scroll::decode(value)?,
                None => composer_collapse_on_scroll::default(),
            },
            composer_rich_text_enabled: match value.remove("composerRichTextEnabled") {
                Some(value) => composer_rich_text_enabled::decode(value)?,
                None => composer_rich_text_enabled::default(),
            },
            send_shortcut: match value.remove("sendShortcut") {
                Some(value) => send_shortcut::decode(value)?,
                None => send_shortcut::default(),
            },
            follow_up_behavior: match value.remove("followUpBehavior") {
                Some(value) => follow_up_behavior::decode(value)?,
                None => follow_up_behavior::default(),
            },
            proactive_panels_enabled: match value.remove("proactivePanelsEnabled") {
                Some(value) => proactive_panels_enabled::decode(value)?,
                None => proactive_panels_enabled::default(),
            },
            show_skills_in_slash_menu: match value.remove("showSkillsInSlashMenu") {
                Some(value) => show_skills_in_slash_menu::decode(value)?,
                None => show_skills_in_slash_menu::default(),
            },
            legacy_sidebar_enabled: match value.remove("legacySidebarEnabled") {
                Some(value) => legacy_sidebar_enabled::decode(value)?,
                None => legacy_sidebar_enabled::default(),
            },
            sidebar_working_shelf_enabled: match value.remove("sidebarWorkingShelfEnabled") {
                Some(value) => sidebar_working_shelf_enabled::decode(value)?,
                None => sidebar_working_shelf_enabled::default(),
            },
            sidebar_project_grouping_mode: match value.remove("sidebarProjectGroupingMode") {
                Some(value) => sidebar_project_grouping_mode::decode(value)?,
                None => sidebar_project_grouping_mode::default(),
            },
            sidebar_project_sort_order: match value.remove("sidebarProjectSortOrder") {
                Some(value) => sidebar_project_sort_order::decode(value)?,
                None => sidebar_project_sort_order::default(),
            },
            sidebar_thread_sort_order: match value.remove("sidebarThreadSortOrder") {
                Some(value) => sidebar_thread_sort_order::decode(value)?,
                None => sidebar_thread_sort_order::default(),
            },
            sidebar_thread_preview_count: match value.remove("sidebarThreadPreviewCount") {
                Some(value) => sidebar_thread_preview_count::decode(value)?,
                None => sidebar_thread_preview_count::default(),
            },
            timestamp_format: match value.remove("timestampFormat") {
                Some(value) => timestamp_format::decode(value)?,
                None => timestamp_format::default(),
            },
            snap_shot_enabled: match value.remove("snapShotEnabled") {
                Some(value) => snap_shot_enabled::decode(value)?,
                None => snap_shot_enabled::default(),
            },
            snap_shot_include_accessibility: match value.remove("snapShotIncludeAccessibility") {
                Some(value) => snap_shot_include_accessibility::decode(value)?,
                None => snap_shot_include_accessibility::default(),
            },
            snap_shot_shortcut: match value.remove("snapShotShortcut") {
                Some(value) => snap_shot_shortcut::decode(value)?,
                None => snap_shot_shortcut::default(),
            },
            snap_shot_play_sound: match value.remove("snapShotPlaySound") {
                Some(value) => snap_shot_play_sound::decode(value)?,
                None => snap_shot_play_sound::default(),
            },
            snap_shot_sound: match value.remove("snapShotSound") {
                Some(value) => snap_shot_sound::decode(value)?,
                None => snap_shot_sound::default(),
            },
            snap_shot_flash: match value.remove("snapShotFlash") {
                Some(value) => snap_shot_flash::decode(value)?,
                None => snap_shot_flash::default(),
            },
            snap_shot_animations: match value.remove("snapShotAnimations") {
                Some(value) => snap_shot_animations::decode(value)?,
                None => snap_shot_animations::default(),
            },
            word_wrap: match value.remove("wordWrap") {
                Some(value) => word_wrap::decode(value)?,
                None => word_wrap::default(),
            },
        })
    }
}

pub type ClientSettingsSchema = ClientSettings;
impl Default for ClientSettings {
    fn default() -> Self {
        serde_json::from_str("{}").expect("source-backed client settings defaults")
    }
}

fn decode_quit_confirmation_setting<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<QuitConfirmationMode, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    match value {
        serde_json::Value::Null => Ok(QuitConfirmationMode::Hold),
        serde_json::Value::Bool(value) => Ok(if value {
            QuitConfirmationMode::Hold
        } else {
            QuitConfirmationMode::Direct
        }),
        value => confirm_quit::decode(value).map_err(serde::de::Error::custom),
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "BTreeMap<String, serde_json::Value>")]
pub struct ModelFavorite {
    pub provider: ProviderInstanceId,
    pub model: TrimmedNonEmptyString,
}
impl TryFrom<BTreeMap<String, serde_json::Value>> for ModelFavorite {
    type Error = serde_json::Error;
    fn try_from(mut value: BTreeMap<String, serde_json::Value>) -> Result<Self, Self::Error> {
        Ok(Self {
            provider: serde_json::from_value(
                value.remove("provider").unwrap_or(serde_json::Value::Null),
            )?,
            model: serde_json::from_value(
                value.remove("model").unwrap_or(serde_json::Value::Null),
            )?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "BTreeMap<String, serde_json::Value>")]
pub struct ProviderModelPreferences {
    #[serde(default, deserialize_with = "decode_default_string_array")]
    pub hidden_models: Vec<String>,
    #[serde(default, deserialize_with = "decode_default_string_array")]
    pub model_order: Vec<String>,
}
impl TryFrom<BTreeMap<String, serde_json::Value>> for ProviderModelPreferences {
    type Error = serde_json::Error;
    fn try_from(mut value: BTreeMap<String, serde_json::Value>) -> Result<Self, Self::Error> {
        Ok(Self {
            hidden_models: value
                .remove("hiddenModels")
                .map(decode_default_string_array)
                .transpose()?
                .unwrap_or_default(),
            model_order: value
                .remove("modelOrder")
                .map(decode_default_string_array)
                .transpose()?
                .unwrap_or_default(),
        })
    }
}

fn decode_default_string_array<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnifiedSettings {
    #[serde(flatten)]
    pub server: ServerSettings,
    #[serde(flatten)]
    pub client: ClientSettings,
}
impl Default for UnifiedSettings {
    fn default() -> Self {
        Self {
            server: ServerSettings::default(),
            client: ClientSettings::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "BTreeMap<String, serde_json::Value>")]
pub struct ClientSettingsPatch {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub notification_mode: Option<NotificationMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub in_app_notifications_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub diff_color_scheme: Option<DiffColorScheme>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub chat_width: Option<ChatWidth>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub load_balancing_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub load_balancing_weights: Option<LoadBalancingWeights>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub appearance_contrast: Option<AppearanceContrast>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub panel_animation_duration_ms: Option<PanelAnimationDurationMs>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_default_viewport: Option<PreviewViewportSetting>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_default_zoom_factor: Option<PreviewZoomFactor>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_default_appearance: Option<PreviewAppearancePreference>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_recording_frame_rate: Option<BrowserRecordingFrameRate>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_recording_show_key_presses: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_recording_show_mouse_presses: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_link_target: Option<BrowserLinkTarget>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_auto_show_floating_preview: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_profiles: Option<Vec<BrowserProfile>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub browser_default_profile_id: Option<BrowserProfileId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub confirm_quit: Option<QuitConfirmationMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub confirm_thread_archive: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub confirm_thread_delete: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub confirm_thread_unpin: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub diff_files_collapsed: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub diff_ignore_whitespace: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub diff_layout: Option<DiffLayout>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub environment_identification_mode: Option<EnvironmentIdentificationMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub glass_opacity: Option<GlassOpacity>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_size_interface: Option<InterfaceFontSize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_size_prompt: Option<PromptFontSize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_size_code: Option<CodeFontSize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_size_terminal: Option<TerminalFontSize>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_family_code: Option<FontFamilyPreference>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_family_composer: Option<FontFamilyPreference>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_family_sans: Option<FontFamilyPreference>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_family_terminal: Option<FontFamilyPreference>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub font_smoothing: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub onboarding_completed_at: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub persist_composer_context_strip: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub favorites: Option<Vec<ModelFavorite>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub provider_model_preferences: Option<BTreeMap<ProviderInstanceId, ProviderModelPreferences>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub pull_request_merge_method_overrides:
        Option<BTreeMap<TrimmedNonEmptyString, PullRequestMergeMethod>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub plan_mode_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub context_window_meter_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub composer_collapse_on_scroll: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub composer_rich_text_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub send_shortcut: Option<SendShortcut>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub follow_up_behavior: Option<FollowUpBehavior>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub proactive_panels_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub show_skills_in_slash_menu: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub legacy_sidebar_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_working_shelf_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_project_grouping_mode: Option<SidebarProjectGroupingMode>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_project_grouping_overrides:
        Option<BTreeMap<TrimmedNonEmptyString, SidebarProjectGroupingMode>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_project_sort_order: Option<SidebarProjectSortOrder>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_thread_sort_order: Option<SidebarThreadSortOrder>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub sidebar_thread_preview_count: Option<SidebarThreadPreviewCount>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub timestamp_format: Option<TimestampFormat>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snap_shot_enabled: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snap_shot_include_accessibility: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snap_shot_shortcut: Option<SnapShotShortcut>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snap_shot_play_sound: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snap_shot_sound: Option<SnapShotSound>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snap_shot_flash: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub snap_shot_animations: Option<bool>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional"
    )]
    pub word_wrap: Option<bool>,
}
impl TryFrom<BTreeMap<String, serde_json::Value>> for ClientSettingsPatch {
    type Error = serde_json::Error;
    fn try_from(mut value: BTreeMap<String, serde_json::Value>) -> Result<Self, Self::Error> {
        Ok(Self {
            pull_request_merge_method_overrides: value
                .remove("pullRequestMergeMethodOverrides")
                .map(serde_json::from_value)
                .transpose()?,
            sidebar_project_grouping_overrides: value
                .remove("sidebarProjectGroupingOverrides")
                .map(serde_json::from_value)
                .transpose()?,
            notification_mode: value
                .remove("notificationMode")
                .map(serde_json::from_value)
                .transpose()?,
            in_app_notifications_enabled: value
                .remove("inAppNotificationsEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            diff_color_scheme: value
                .remove("diffColorScheme")
                .map(serde_json::from_value)
                .transpose()?,
            chat_width: value
                .remove("chatWidth")
                .map(serde_json::from_value)
                .transpose()?,
            load_balancing_enabled: value
                .remove("loadBalancingEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            load_balancing_weights: value
                .remove("loadBalancingWeights")
                .map(serde_json::from_value)
                .transpose()?,
            appearance_contrast: value
                .remove("appearanceContrast")
                .map(serde_json::from_value)
                .transpose()?,
            panel_animation_duration_ms: value
                .remove("panelAnimationDurationMs")
                .map(serde_json::from_value)
                .transpose()?,
            browser_default_viewport: value
                .remove("browserDefaultViewport")
                .map(serde_json::from_value)
                .transpose()?,
            browser_default_zoom_factor: value
                .remove("browserDefaultZoomFactor")
                .map(serde_json::from_value)
                .transpose()?,
            browser_default_appearance: value
                .remove("browserDefaultAppearance")
                .map(serde_json::from_value)
                .transpose()?,
            browser_recording_frame_rate: value
                .remove("browserRecordingFrameRate")
                .map(serde_json::from_value)
                .transpose()?,
            browser_recording_show_key_presses: value
                .remove("browserRecordingShowKeyPresses")
                .map(serde_json::from_value)
                .transpose()?,
            browser_recording_show_mouse_presses: value
                .remove("browserRecordingShowMousePresses")
                .map(serde_json::from_value)
                .transpose()?,
            browser_link_target: value
                .remove("browserLinkTarget")
                .map(serde_json::from_value)
                .transpose()?,
            browser_auto_show_floating_preview: value
                .remove("browserAutoShowFloatingPreview")
                .map(serde_json::from_value)
                .transpose()?,
            browser_profiles: value
                .remove("browserProfiles")
                .map(serde_json::from_value)
                .transpose()?,
            browser_default_profile_id: value
                .remove("browserDefaultProfileId")
                .map(serde_json::from_value)
                .transpose()?,
            confirm_quit: value
                .remove("confirmQuit")
                .map(serde_json::from_value)
                .transpose()?,
            confirm_thread_archive: value
                .remove("confirmThreadArchive")
                .map(serde_json::from_value)
                .transpose()?,
            confirm_thread_delete: value
                .remove("confirmThreadDelete")
                .map(serde_json::from_value)
                .transpose()?,
            confirm_thread_unpin: value
                .remove("confirmThreadUnpin")
                .map(serde_json::from_value)
                .transpose()?,
            diff_files_collapsed: value
                .remove("diffFilesCollapsed")
                .map(serde_json::from_value)
                .transpose()?,
            diff_ignore_whitespace: value
                .remove("diffIgnoreWhitespace")
                .map(serde_json::from_value)
                .transpose()?,
            diff_layout: value
                .remove("diffLayout")
                .map(serde_json::from_value)
                .transpose()?,
            environment_identification_mode: value
                .remove("environmentIdentificationMode")
                .map(serde_json::from_value)
                .transpose()?,
            glass_opacity: value
                .remove("glassOpacity")
                .map(serde_json::from_value)
                .transpose()?,
            font_size_interface: value
                .remove("fontSizeInterface")
                .map(serde_json::from_value)
                .transpose()?,
            font_size_prompt: value
                .remove("fontSizePrompt")
                .map(serde_json::from_value)
                .transpose()?,
            font_size_code: value
                .remove("fontSizeCode")
                .map(serde_json::from_value)
                .transpose()?,
            font_size_terminal: value
                .remove("fontSizeTerminal")
                .map(serde_json::from_value)
                .transpose()?,
            font_family_code: value
                .remove("fontFamilyCode")
                .map(serde_json::from_value)
                .transpose()?,
            font_family_composer: value
                .remove("fontFamilyComposer")
                .map(serde_json::from_value)
                .transpose()?,
            font_family_sans: value
                .remove("fontFamilySans")
                .map(serde_json::from_value)
                .transpose()?,
            font_family_terminal: value
                .remove("fontFamilyTerminal")
                .map(serde_json::from_value)
                .transpose()?,
            font_smoothing: value
                .remove("fontSmoothing")
                .map(serde_json::from_value)
                .transpose()?,
            onboarding_completed_at: value
                .remove("onboardingCompletedAt")
                .map(serde_json::from_value)
                .transpose()?,
            persist_composer_context_strip: value
                .remove("persistComposerContextStrip")
                .map(serde_json::from_value)
                .transpose()?,
            favorites: value
                .remove("favorites")
                .map(serde_json::from_value)
                .transpose()?,
            provider_model_preferences: value
                .remove("providerModelPreferences")
                .map(serde_json::from_value)
                .transpose()?,
            plan_mode_enabled: value
                .remove("planModeEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            context_window_meter_enabled: value
                .remove("contextWindowMeterEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            composer_collapse_on_scroll: value
                .remove("composerCollapseOnScroll")
                .map(serde_json::from_value)
                .transpose()?,
            composer_rich_text_enabled: value
                .remove("composerRichTextEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            send_shortcut: value
                .remove("sendShortcut")
                .map(serde_json::from_value)
                .transpose()?,
            follow_up_behavior: value
                .remove("followUpBehavior")
                .map(serde_json::from_value)
                .transpose()?,
            proactive_panels_enabled: value
                .remove("proactivePanelsEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            show_skills_in_slash_menu: value
                .remove("showSkillsInSlashMenu")
                .map(serde_json::from_value)
                .transpose()?,
            legacy_sidebar_enabled: value
                .remove("legacySidebarEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            sidebar_working_shelf_enabled: value
                .remove("sidebarWorkingShelfEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            sidebar_project_grouping_mode: value
                .remove("sidebarProjectGroupingMode")
                .map(serde_json::from_value)
                .transpose()?,
            sidebar_project_sort_order: value
                .remove("sidebarProjectSortOrder")
                .map(serde_json::from_value)
                .transpose()?,
            sidebar_thread_sort_order: value
                .remove("sidebarThreadSortOrder")
                .map(serde_json::from_value)
                .transpose()?,
            sidebar_thread_preview_count: value
                .remove("sidebarThreadPreviewCount")
                .map(serde_json::from_value)
                .transpose()?,
            timestamp_format: value
                .remove("timestampFormat")
                .map(serde_json::from_value)
                .transpose()?,
            snap_shot_enabled: value
                .remove("snapShotEnabled")
                .map(serde_json::from_value)
                .transpose()?,
            snap_shot_include_accessibility: value
                .remove("snapShotIncludeAccessibility")
                .map(serde_json::from_value)
                .transpose()?,
            snap_shot_shortcut: value
                .remove("snapShotShortcut")
                .map(serde_json::from_value)
                .transpose()?,
            snap_shot_play_sound: value
                .remove("snapShotPlaySound")
                .map(serde_json::from_value)
                .transpose()?,
            snap_shot_sound: value
                .remove("snapShotSound")
                .map(serde_json::from_value)
                .transpose()?,
            snap_shot_flash: value
                .remove("snapShotFlash")
                .map(serde_json::from_value)
                .transpose()?,
            snap_shot_animations: value
                .remove("snapShotAnimations")
                .map(serde_json::from_value)
                .transpose()?,
            word_wrap: value
                .remove("wordWrap")
                .map(serde_json::from_value)
                .transpose()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreviewZoomFactor(pub f64);
impl<'de> Deserialize<'de> for PreviewZoomFactor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(d)?;
        if [
            0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0,
            5.0,
        ]
        .contains(&value)
        {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("unsupported preview zoom level"))
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BrowserRecordingFrameRate(pub u8);
impl<'de> Deserialize<'de> for BrowserRecordingFrameRate {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = f64::deserialize(d)?;
        if value == 30.0 || value == 60.0 {
            Ok(Self(value as u8))
        } else {
            Err(serde::de::Error::custom(
                "expected 30 or 60 frames per second",
            ))
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(try_from = "BTreeMap<String, serde_json::Value>")]
pub struct BrowserProfile {
    pub id: BrowserProfileId,
    pub name: BrowserProfileName,
    pub kind: BrowserProfileKind,
}
impl TryFrom<BTreeMap<String, serde_json::Value>> for BrowserProfile {
    type Error = serde_json::Error;
    fn try_from(mut value: BTreeMap<String, serde_json::Value>) -> Result<Self, Self::Error> {
        Ok(Self {
            id: serde_json::from_value(value.remove("id").unwrap_or(serde_json::Value::Null))?,
            name: serde_json::from_value(value.remove("name").unwrap_or(serde_json::Value::Null))?,
            kind: serde_json::from_value(value.remove("kind").unwrap_or(serde_json::Value::Null))?,
        })
    }
}

fn validate_browser_profile_id(value: &str) -> Result<(), ValidationError> {
    if !value.is_empty()
        && value.encode_utf16().count() <= 64
        && !value
            .chars()
            .any(|c| matches!(c,'\u{0000}'..='\u{001f}'|'\u{007f}'..='\u{009f}'))
    {
        Ok(())
    } else {
        Err(ValidationError {
            expected: "a browser profile ID without control characters, at most 64 UTF-16 units",
        })
    }
}
crate::base::string_type!(BrowserProfileId, validate_browser_profile_id);
pub type BrowserProfileName = BoundedTrimmedString<48>;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "_tag",
    rename_all = "lowercase",
    try_from = "PreviewViewportWire"
)]
pub enum PreviewViewportSetting {
    Fill,
    Freeform {
        width: RangeInt<240, 3840>,
        height: RangeInt<240, 3840>,
    },
    Preset {
        width: RangeInt<240, 3840>,
        height: RangeInt<240, 3840>,
        #[serde(rename = "presetId")]
        preset_id: StoredPreviewViewportPresetId,
    },
}
#[derive(Deserialize)]
#[serde(tag = "_tag", rename_all = "lowercase")]
enum PreviewViewportWire {
    Fill,
    Freeform {
        width: RangeInt<240, 3840>,
        height: RangeInt<240, 3840>,
    },
    Preset {
        width: RangeInt<240, 3840>,
        height: RangeInt<240, 3840>,
        #[serde(rename = "presetId")]
        preset_id: StoredPreviewViewportPresetId,
    },
}
impl TryFrom<PreviewViewportWire> for PreviewViewportSetting {
    type Error = &'static str;
    fn try_from(value: PreviewViewportWire) -> Result<Self, Self::Error> {
        let (width, height) = match &value {
            PreviewViewportWire::Fill => (0, 0),
            PreviewViewportWire::Freeform { width, height }
            | PreviewViewportWire::Preset { width, height, .. } => (width.0, height.0),
        };
        if width * height > 3840 * 2160 {
            return Err("viewport area must not exceed 8294400 pixels");
        }
        Ok(match value {
            PreviewViewportWire::Fill => Self::Fill,
            PreviewViewportWire::Freeform { width, height } => Self::Freeform { width, height },
            PreviewViewportWire::Preset {
                width,
                height,
                preset_id,
            } => Self::Preset {
                width,
                height,
                preset_id,
            },
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SnapShotShortcut {
    ModifierPair(SnapShotModifierPairShortcut),
    KeyChord(SnapShotKeyChord),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum SnapShotModifierPairShortcut {
    #[serde(rename = "both-shift-keys")]
    BothShiftKeys,
    #[serde(rename = "modifier-pair")]
    ModifierPair { modifier: SnapShotModifier },
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SnapShotKeyChord(pub KeybindingShortcut);
impl<'de> Deserialize<'de> for SnapShotKeyChord {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom(
                "snapshot shortcut must be an object",
            ));
        }
        let value: KeybindingShortcut =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        if value.meta_key || value.ctrl_key || value.shift_key || value.alt_key || value.mod_key {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "snapshot shortcut requires a modifier",
            ))
        }
    }
}
/// Built-in profiles always precede user profiles; duplicate IDs use the first
/// entry and custom incognito profiles are canonicalized to persistent.
pub fn resolve_browser_profiles(profiles: &[BrowserProfile]) -> Vec<BrowserProfile> {
    let mut resolved:Vec<BrowserProfile>=serde_json::from_str(r#"[{"id":"default","name":"Default","kind":"persistent"},{"id":"incognito","name":"Incognito","kind":"incognito"}]"#).expect("source built-in browser profiles");
    let mut seen: std::collections::BTreeSet<BrowserProfileId> =
        resolved.iter().map(|p| p.id.clone()).collect();
    for profile in profiles {
        if seen.insert(profile.id.clone()) {
            let mut profile = profile.clone();
            profile.kind = BrowserProfileKind::Persistent;
            resolved.push(profile);
        }
    }
    resolved
}
impl SnapShotModifierPairShortcut {
    pub fn modifier(&self) -> SnapShotModifier {
        match self {
            Self::BothShiftKeys => SnapShotModifier::Shift,
            Self::ModifierPair { modifier } => *modifier,
        }
    }
}
pub fn snap_shot_modifier_pair_label(modifier: SnapShotModifier, apple: bool) -> String {
    let label = match modifier {
        SnapShotModifier::Shift => "Shift",
        SnapShotModifier::Meta => {
            if apple {
                "Command"
            } else {
                "Super"
            }
        }
        SnapShotModifier::Control => {
            if apple {
                "Control"
            } else {
                "Ctrl"
            }
        }
        SnapShotModifier::Alt => {
            if apple {
                "Option"
            } else {
                "Alt"
            }
        }
    };
    format!("{label} + {label}")
}

impl Serialize for PreviewZoomFactor {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if self.0.fract() == 0.0 {
            s.serialize_u64(self.0 as u64)
        } else {
            s.serialize_f64(self.0)
        }
    }
}
