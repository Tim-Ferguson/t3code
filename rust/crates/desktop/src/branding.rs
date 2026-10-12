//! The name this build goes by (`resolveDesktopAppBranding` in `DesktopEnvironment.ts`, `getAppBranding` of the original's bridge): "T3 Code"
//! and the channel it is on, as the window title, the application menu and the About panel carry it.

pub const BASE_NAME: &str = "T3 Code";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branding {
    pub base_name: &'static str,
    pub stage_label: &'static str,
    /// "T3 Code (Dev)".
    pub display_name: String,
}

/// `stage_label` is the channel (`t3_client::stage::app_stage_label`: Dev, Nightly or Alpha).
pub fn resolve(stage_label: &'static str) -> Branding {
    Branding { base_name: BASE_NAME, stage_label, display_name: format!("{BASE_NAME} ({stage_label})") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_display_name_is_the_name_with_the_channel_in_brackets() {
        let branding = resolve("Nightly");
        assert_eq!((branding.base_name, branding.stage_label, branding.display_name.as_str()), ("T3 Code", "Nightly", "T3 Code (Nightly)"));
        assert_eq!(resolve(t3_client::stage::app_stage_label()).display_name, format!("T3 Code ({})", t3_client::stage::app_stage_label()));
    }
}
