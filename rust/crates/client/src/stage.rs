//! Which release channel an environment is, and how the sidebar marks it (`branding.logic.ts`, `SidebarStageBackdrop.tsx`,
//! `resolveEnvironmentIdentificationMode` of `useSettings.ts`). A nightly server shows the nightly artwork whatever
//! this app is; otherwise the app's own channel decides.
use regex::Regex;
use std::sync::LazyLock;
use t3_contracts::EnvironmentIdentificationMode as Mode;

static NIGHTLY_SERVER_VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^-+]+-(?:nightly|preview)\.\d{8}\.\d+$").expect("nightly version pattern"));

/// The channel this build calls itself (`APP_STAGE_LABEL`): a debug build is "Dev", a build whose version is a nightly one is
/// "Nightly", any other is "Alpha".
pub fn app_stage_label() -> &'static str {
    if cfg!(debug_assertions) {
        "Dev"
    } else if NIGHTLY_SERVER_VERSION.is_match(env!("CARGO_PKG_VERSION")) {
        "Nightly"
    } else {
        "Alpha"
    }
}

/// `resolveServerBackedAppStageLabel`: "Nightly" when the primary server's version is a nightly one, else the app's own label.
pub fn stage_label(primary_server_version: Option<&str>, fallback: &str) -> String {
    match primary_server_version {
        Some(version) if NIGHTLY_SERVER_VERSION.is_match(version) => "Nightly".to_owned(),
        _ => fallback.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Nightly,
    Dev,
}

/// `resolveSidebarStageBackdropVariant`: the artwork for a channel, none when it is switched off.
pub fn backdrop_variant(label: &str, enabled: bool) -> Option<Variant> {
    if !enabled {
        return None;
    }
    match label.trim().to_lowercase().as_str() {
        "nightly" => Some(Variant::Nightly),
        "dev" => Some(Variant::Dev),
        _ => None,
    }
}

/// `resolveEnvironmentIdentificationPillLabel`: the text of the version pill, for the channels that have one.
pub fn pill_label(label: &str) -> Option<&'static str> {
    match label.trim().to_lowercase().as_str() {
        "dev" => Some("Dev"),
        "nightly" => Some("Nightly"),
        _ => None,
    }
}

/// `resolveEnvironmentIdentificationMode`: what is drawn. Nothing until the preferences are read (so a saved pill or none is not
/// briefly artwork); and a palette theme that is not one of the reviewed built-ins gets the pill instead of artwork.
pub fn resolve_mode(mode: Mode, hydrated: bool, palette_theme_active: bool, palette_theme_allows_artwork: bool) -> Mode {
    if !hydrated {
        return Mode::None;
    }
    if palette_theme_active && !palette_theme_allows_artwork && mode == Mode::Artwork { Mode::Pill } else { mode }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nightly_server_makes_the_environment_nightly_whatever_the_app_is() {
        assert_eq!(stage_label(Some("0.0.23-nightly.20260401.5"), "Alpha"), "Nightly");
        assert_eq!(stage_label(Some("0.0.23-preview.20260401.5"), "Dev"), "Nightly");
        assert_eq!(stage_label(Some("0.0.23"), "Dev"), "Dev");
        assert_eq!(stage_label(Some("0.0.23-nightly.2026.5"), "Alpha"), "Alpha");
        assert_eq!(stage_label(None, "Alpha"), "Alpha");
    }

    #[test]
    fn only_dev_and_nightly_are_marked() {
        assert_eq!(backdrop_variant(" Dev ", true), Some(Variant::Dev));
        assert_eq!(backdrop_variant("NIGHTLY", true), Some(Variant::Nightly));
        assert_eq!(backdrop_variant("Dev", false), None);
        assert_eq!(backdrop_variant("Alpha", true), None);
        assert_eq!(pill_label("dev"), Some("Dev"));
        assert_eq!(pill_label("Nightly"), Some("Nightly"));
        assert_eq!(pill_label("Alpha"), None);
        assert_eq!(pill_label("Latest"), None);
    }

    #[test]
    fn artwork_waits_for_the_preferences_and_gives_way_to_the_pill_on_a_theme_without_reviewed_art() {
        assert_eq!(resolve_mode(Mode::Artwork, false, false, true), Mode::None);
        assert_eq!(resolve_mode(Mode::Artwork, true, false, false), Mode::Artwork);
        assert_eq!(resolve_mode(Mode::Artwork, true, true, true), Mode::Artwork);
        assert_eq!(resolve_mode(Mode::Artwork, true, true, false), Mode::Pill);
        assert_eq!(resolve_mode(Mode::None, true, true, false), Mode::None);
        assert_eq!(resolve_mode(Mode::Pill, true, true, false), Mode::Pill);
    }
}
