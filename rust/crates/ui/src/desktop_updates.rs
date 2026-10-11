//! The desktop app's own update, on the screen: the sidebar's update pill and the version and update track rows of the
//! settings (`SidebarUpdatePill.tsx`, `AboutVersionSection` of `SettingsPanels.tsx`). This app has no update feed, so its
//! state is the original's "disabled" one: the pill is not drawn, the check button is off and says why, and the track
//! is a choice that is kept for the day a feed exists. Nothing here downloads, installs or relaunches anything.
//! The server's own requests to update the app are answered in `t3_desktop::control_channel`.
use dioxus::prelude::*;
use t3_desktop::updates::{self, Channel, Environment, RuntimeInfo, State};

/// The update state the window shows. With no feed the app never leaves the disabled state.
pub(crate) fn current_state() -> State {
    state_at(crate::desktop_shell::settings_path().as_deref())
}
fn state_at(settings: Option<&std::path::Path>) -> State {
    let version = env!("CARGO_PKG_VERSION");
    let channel = settings.map_or_else(|| updates::default_channel(version), |path| updates::channel_in_force(path, version));
    updates::unavailable(version, &RuntimeInfo::current(), channel)
}
/// Why updating is not possible here, in the original's words.
pub(crate) fn disabled_reason() -> Option<&'static str> {
    updates::disabled_reason(&Environment { has_update_feed_config: false, is_development: cfg!(debug_assertions), is_packaged: !cfg!(debug_assertions), disabled_by_env: false })
}
/// What the server's requests to update the app are answered with.
pub(crate) fn answers(settings: &std::path::Path) -> t3_desktop::control_channel::Answers {
    t3_desktop::control_channel::Answers { state: state_at(Some(settings)), reason: disabled_reason().map(str::to_owned) }
}

/// The pill over the sidebar when an update needs the person. Drawn for the states that have something to do or say; with no
/// feed that is never.
#[component]
pub(crate) fn Pill() -> Element {
    let state = current_state();
    if !updates::shows_pill(&state) {
        return rsx! {};
    }
    let label = match updates::button_action(&state) {
        updates::ButtonAction::Install => "Restart to update",
        updates::ButtonAction::Download => "Update available",
        updates::ButtonAction::None => "Update",
    };
    rsx! { div { class: "update-pill", role: "status", title: "{updates::button_tooltip(&state)}", "{label}" } }
}

/// The version row and the update track row of the About settings.
#[component]
pub(crate) fn Rows() -> Element {
    let mut state = use_signal(current_state);
    let mut failure = use_signal(|| None::<String>);
    let snapshot = state.read().clone();
    let reason = disabled_reason();
    let checkable = updates::can_check(&snapshot);
    let selected = if snapshot.channel == Channel::Nightly { "nightly" } else { "latest" };
    rsx! {
        section { class: "sc-section", "aria-label": "About",
            h2 { "About" }
            div { class: "field-row",
                div { strong { "Version {snapshot.current_version}" } p { class: "muted", "Current version of the application." } }
                button { r#type: "button", "aria-label": "Check for Updates", disabled: !checkable, title: "{reason.unwrap_or_default()}", "Check for Updates" }
            }
            if let Some(reason) = reason { p { class: "muted", role: "status", "{reason}" } }
            if updates::arm64_intel_build(&snapshot) { p { class: "muted", "This Mac has Apple Silicon, but T3 Code is still running the Intel build under Rosetta. The next app update will replace it with the native Apple Silicon build." } }
            div { class: "field-row",
                div { strong { "Update track" } p { class: "muted", "Use stable releases or nightly builds. Switch back anytime." } }
                select { "aria-label": "Update track", value: "{selected}",
                    onchange: move |event| {
                        let channel = if event.value() == "nightly" { Channel::Nightly } else { Channel::Latest };
                        let Some(path) = crate::desktop_shell::settings_path() else { return };
                        match updates::write_channel(&path, channel) {
                            Ok(()) => { failure.set(None); state.set(current_state()); }
                            Err(error) => failure.set(Some(format!("Could not change the update track. {error}"))),
                        }
                    },
                    option { value: "latest", selected: selected == "latest", "Stable" }
                    option { value: "nightly", selected: selected == "nightly", "Nightly" }
                }
            }
            if let Some(message) = failure.read().clone() { p { class: "error-banner", role: "alert", "{message}" } }
        }
    }
}
