//! The desktop app's own update, as pure logic. Port of `updates/updateMachine.ts` (the state the updater is in and how each
//! event moves it), `updateChannels.ts`, `remoteUpdateFlow.ts` (what a run the server asked for does next) and the parts of
//! `DesktopUpdates.ts` that decide whether updating is possible at all.
//!
//! This app has no update feed: `Disabled` is the only state it reaches, and a request over the telemetry control channel is
//! answered as the original answers it when no feed is configured (`unavailable_report`). The rest of the machine is here so a
//! feed can be added without changing what the server and the client see; nothing in this module downloads or installs anything.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Disabled,
    Idle,
    Checking,
    UpToDate,
    Available,
    Downloading,
    Downloaded,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Latest,
    Nightly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErrorContext {
    Check,
    Download,
    Install,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseNote {
    pub version: String,
    pub items: Vec<String>,
    pub total_items: u64,
}

/// `DesktopUpdateState`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub enabled: bool,
    pub status: Status,
    pub channel: Channel,
    pub current_version: String,
    pub host_arch: String,
    pub app_arch: String,
    pub running_under_arm64_translation: bool,
    pub available_version: Option<String>,
    pub downloaded_version: Option<String>,
    pub release_notes: Vec<ReleaseNote>,
    pub omitted_release_count: u64,
    pub download_percent: Option<f64>,
    pub checked_at: Option<String>,
    pub message: Option<String>,
    pub error_context: Option<ErrorContext>,
    pub can_retry: bool,
}

/// `DesktopRuntimeInfo`: the machine's architecture and the app's, which differ under translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub host_arch: String,
    pub app_arch: String,
    pub running_under_arm64_translation: bool,
}
impl RuntimeInfo {
    /// What this build is running as, on the machine it runs on.
    pub fn current() -> Self {
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            other => other,
        };
        Self { host_arch: arch.to_owned(), app_arch: arch.to_owned(), running_under_arm64_translation: false }
    }
}

const NIGHTLY_VERSION: &str = r"^[^-+]+-nightly\.\d{8}\.\d+$";

/// A version shaped `1.2.3-nightly.20260101.4` (`NIGHTLY_VERSION_PATTERN`), without a regular-expression engine.
fn matches_train(version: &str, trains: &[&str]) -> bool {
    let Some((base, rest)) = version.split_once('-') else { return false };
    if base.is_empty() || base.contains('+') {
        return false;
    }
    let Some((train, build)) = rest.split_once('.') else { return false };
    let Some((date, number)) = build.split_once('.') else { return false };
    trains.contains(&train) && date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) && !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
}
/// `isNightlyDesktopVersion`: nightly and preview builds share the nightly branding.
pub fn is_nightly_version(version: &str) -> bool {
    matches_train(version, &["nightly", "preview"])
}
/// `resolveDefaultDesktopUpdateChannel`: only a true nightly follows the nightly channel by default.
pub fn default_channel(app_version: &str) -> Channel {
    let _ = NIGHTLY_VERSION;
    if matches_train(app_version, &["nightly"]) { Channel::Nightly } else { Channel::Latest }
}

/// `createInitialDesktopUpdateState`.
pub fn initial(current_version: &str, runtime: &RuntimeInfo, channel: Channel) -> State {
    State {
        enabled: false,
        status: Status::Disabled,
        channel,
        current_version: current_version.to_owned(),
        host_arch: runtime.host_arch.clone(),
        app_arch: runtime.app_arch.clone(),
        running_under_arm64_translation: runtime.running_under_arm64_translation,
        available_version: None,
        downloaded_version: None,
        release_notes: vec![],
        omitted_release_count: 0,
        download_percent: None,
        checked_at: None,
        message: None,
        error_context: None,
        can_retry: false,
    }
}

pub fn on_check_start(state: &State, checked_at: &str) -> State {
    let downloaded = state.downloaded_version.is_some();
    State {
        status: Status::Checking,
        checked_at: Some(checked_at.to_owned()),
        release_notes: if downloaded { state.release_notes.clone() } else { vec![] },
        omitted_release_count: if downloaded { state.omitted_release_count } else { 0 },
        message: None,
        download_percent: downloaded.then_some(100.0),
        error_context: None,
        can_retry: false,
        ..state.clone()
    }
}

pub fn on_check_failure(state: &State, message: &str, checked_at: &str) -> State {
    if state.downloaded_version.is_some() {
        return State { status: Status::Downloaded, message: None, checked_at: Some(checked_at.to_owned()), download_percent: Some(100.0), error_context: None, can_retry: true, ..state.clone() };
    }
    State { status: Status::Error, message: Some(message.to_owned()), checked_at: Some(checked_at.to_owned()), download_percent: None, error_context: Some(ErrorContext::Check), can_retry: true, ..state.clone() }
}

pub fn on_update_available(state: &State, version: &str, checked_at: &str, release_notes: Vec<ReleaseNote>, omitted_release_count: u64) -> State {
    let downloaded = state.downloaded_version.as_deref() == Some(version);
    let preserve = downloaded && release_notes.is_empty();
    State {
        status: if downloaded { Status::Downloaded } else { Status::Available },
        available_version: Some(version.to_owned()),
        downloaded_version: downloaded.then(|| version.to_owned()),
        release_notes: if preserve { state.release_notes.clone() } else { release_notes },
        omitted_release_count: if preserve { state.omitted_release_count } else { omitted_release_count },
        download_percent: downloaded.then_some(100.0),
        checked_at: Some(checked_at.to_owned()),
        message: None,
        error_context: None,
        can_retry: downloaded,
        ..state.clone()
    }
}

pub fn on_no_update(state: &State, checked_at: &str) -> State {
    if let Some(version) = &state.downloaded_version {
        return State { status: Status::Downloaded, available_version: Some(version.clone()), download_percent: Some(100.0), checked_at: Some(checked_at.to_owned()), message: None, error_context: None, can_retry: true, ..state.clone() };
    }
    State {
        status: Status::UpToDate,
        available_version: None,
        downloaded_version: None,
        release_notes: vec![],
        omitted_release_count: 0,
        download_percent: None,
        checked_at: Some(checked_at.to_owned()),
        message: None,
        error_context: None,
        can_retry: false,
        ..state.clone()
    }
}

pub fn on_download_start(state: &State) -> State {
    State { status: Status::Downloading, download_percent: Some(0.0), message: None, error_context: None, can_retry: false, ..state.clone() }
}

pub fn on_download_failure(state: &State, message: &str) -> State {
    let has_update = state.available_version.is_some();
    State { status: if has_update { Status::Available } else { Status::Error }, message: Some(message.to_owned()), download_percent: None, error_context: Some(ErrorContext::Download), can_retry: has_update, ..state.clone() }
}

pub fn on_download_progress(state: &State, percent: f64) -> State {
    State { status: Status::Downloading, download_percent: Some(percent), message: None, error_context: None, can_retry: false, ..state.clone() }
}

pub fn on_download_complete(state: &State, version: &str) -> State {
    State { status: Status::Downloaded, available_version: Some(version.to_owned()), downloaded_version: Some(version.to_owned()), download_percent: Some(100.0), message: None, error_context: None, can_retry: true, ..state.clone() }
}

pub fn on_install_failure(state: &State, message: &str) -> State {
    State { status: Status::Downloaded, message: Some(message.to_owned()), error_context: Some(ErrorContext::Install), can_retry: true, ..state.clone() }
}

/// What makes updating impossible on this machine, in the original's words (`getAutoUpdateDisabledReason`); none when it is possible.
/// This app ships no update feed, so the first reason is the one it gives.
pub struct Environment {
    pub has_update_feed_config: bool,
    pub is_development: bool,
    pub is_packaged: bool,
    pub disabled_by_env: bool,
}
pub fn disabled_reason(environment: &Environment) -> Option<&'static str> {
    if !environment.has_update_feed_config {
        return Some("Automatic updates are not available because no update feed is configured.");
    }
    if environment.is_development || !environment.is_packaged {
        return Some("Automatic updates are only available in packaged production builds.");
    }
    if environment.disabled_by_env {
        return Some("Automatic updates are disabled by the T3CODE_DISABLE_AUTO_UPDATE setting.");
    }
    None
}

/// The state the app is in with no feed: `createBaseUpdateState(channel, enabled = false, …)`.
pub fn unavailable(current_version: &str, runtime: &RuntimeInfo, channel: Channel) -> State {
    initial(current_version, runtime, channel)
}

// ---- A run the server asked for (`remoteUpdateFlow.ts`) ----

pub const MAX_REMOTE_UPDATE_CHECKS: u32 = 2;
pub const MAX_REMOTE_UPDATE_DOWNLOADS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    ReadyToInstall,
    UpToDate,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Check,
    Download,
    Install,
    Wait,
    Done { outcome: Outcome, reason: Option<String> },
}

/// How many times a run issued each action; the caps stop a check, up-to-date, check loop and endless download retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attempts {
    pub checks: u32,
    pub downloads: u32,
}

fn installable(state: &State) -> bool {
    state.downloaded_version.is_some() && (state.status == Status::Downloaded || (state.status == Status::Error && matches!(state.error_context, None | Some(ErrorContext::Install))))
}

/// `nextRemoteDesktopUpdateStep`.
pub fn next_step(state: &State, attempts: Attempts, disabled_reason: Option<&str>) -> Step {
    let done = |outcome, reason: Option<&str>| Step::Done { outcome, reason: reason.map(str::to_owned) };
    if !state.enabled || state.status == Status::Disabled {
        return done(Outcome::Failed, Some(disabled_reason.unwrap_or("Automatic updates are disabled on this machine.")));
    }
    if installable(state) {
        return Step::Install;
    }
    match state.status {
        Status::Downloading | Status::Checking => Step::Wait,
        Status::Available => {
            if attempts.downloads >= MAX_REMOTE_UPDATE_DOWNLOADS {
                done(Outcome::Failed, Some(state.message.as_deref().unwrap_or("The desktop app failed to download the update.")))
            } else {
                Step::Download
            }
        }
        // Retained from earlier or background checks, so before this run has checked itself they are stale, not terminal.
        Status::UpToDate => if attempts.checks == 0 { Step::Check } else { done(Outcome::UpToDate, None) },
        Status::Error => if attempts.checks == 0 { Step::Check } else { done(Outcome::Failed, Some(state.message.as_deref().unwrap_or("The desktop app update failed."))) },
        _ => if attempts.checks >= MAX_REMOTE_UPDATE_CHECKS { done(Outcome::Failed, Some("The desktop app did not report an update result.")) } else { Step::Check },
    }
}

/// `normalizeRemoteUpdateReason`: the wire field is a trimmed non-empty string, and a blank one would drop the whole report.
pub fn normalize_reason(reason: Option<&str>) -> Option<String> {
    reason.map(str::trim).filter(|reason| !reason.is_empty()).map(str::to_owned)
}

/// A `desktopUpdateStatus` frame for the server (`DesktopUpdateStatusReport`).
pub fn report(state: &State, request_id: Option<&str>, terminal: Option<(Outcome, Option<&str>)>) -> Value {
    let mut frame = json!({"version":1,"type":"desktopUpdateStatus","state":state});
    if let Some(id) = request_id {
        frame["requestId"] = json!(id);
    }
    if let Some((outcome, reason)) = terminal {
        frame["outcome"] = json!(outcome);
        if let Some(reason) = normalize_reason(reason) {
            frame["reason"] = json!(reason);
        }
    }
    frame
}

/// What a request for an update is answered with when there is no feed: the run's first step is already done, as a failure with
/// the reason updating is not possible (`handleRequest` over `nextRemoteDesktopUpdateStep`).
pub fn unavailable_report(request_id: &str, state: &State, reason: Option<&str>) -> Value {
    match next_step(state, Attempts::default(), reason) {
        Step::Done { outcome, reason } => report(state, Some(request_id), Some((outcome, reason.as_deref()))),
        // An enabled updater is not this app's: report it as the original would, a state without a result.
        _ => report(state, Some(request_id), None),
    }
}

/// The failure a commit without a prepared update gets (`DesktopRemoteUpdates.ts`).
pub fn not_prepared_report(request_id: &str, state: &State) -> Value {
    report(state, Some(request_id), Some((Outcome::Failed, Some("This desktop update is no longer prepared."))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> RuntimeInfo {
        RuntimeInfo { host_arch: "arm64".into(), app_arch: "arm64".into(), running_under_arm64_translation: false }
    }
    fn idle() -> State {
        State { enabled: true, status: Status::Idle, ..initial("1.0.0", &runtime(), Channel::Latest) }
    }
    const AT: &str = "2026-01-01T00:00:00.000Z";

    #[test]
    fn the_initial_state_is_disabled_and_the_channel_follows_the_version() {
        let state = initial("1.2.3", &runtime(), default_channel("1.2.3"));
        assert_eq!((state.enabled, state.status, state.channel), (false, Status::Disabled, Channel::Latest));
        assert_eq!(default_channel("1.2.3-nightly.20260101.4"), Channel::Nightly);
        assert_eq!(default_channel("1.2.3-preview.20260101.4"), Channel::Latest, "a preview build is cosmetic: it never follows the feed");
        assert!(is_nightly_version("1.2.3-preview.20260101.4") && is_nightly_version("1.2.3-nightly.20260101.4"));
        assert!(!is_nightly_version("1.2.3") && !is_nightly_version("1.2.3-nightly.2026.4") && !is_nightly_version("1.2.3+x-nightly.20260101.4"));
    }

    #[test]
    fn a_check_runs_to_an_update_that_downloads_and_waits_to_install() {
        let checking = on_check_start(&idle(), AT);
        assert_eq!((checking.status, checking.download_percent), (Status::Checking, None));
        let found = on_update_available(&checking, "1.1.0", AT, vec![ReleaseNote { version: "1.1.0".into(), items: vec!["Fixes".into()], total_items: 1 }], 0);
        assert_eq!((found.status, found.available_version.as_deref(), found.can_retry), (Status::Available, Some("1.1.0"), false));
        let downloading = on_download_progress(&on_download_start(&found), 40.0);
        assert_eq!((downloading.status, downloading.download_percent), (Status::Downloading, Some(40.0)));
        let done = on_download_complete(&downloading, "1.1.0");
        assert_eq!((done.status, done.downloaded_version.as_deref(), done.download_percent, done.can_retry), (Status::Downloaded, Some("1.1.0"), Some(100.0), true));
        // Checking again keeps the download and its notes.
        let again = on_check_start(&done, AT);
        assert_eq!((again.download_percent, again.release_notes.len()), (Some(100.0), 1), "a download held keeps its notes while checking again");
        let nothing = on_no_update(&done, AT);
        assert_eq!((nothing.status, nothing.available_version.as_deref()), (Status::Downloaded, Some("1.1.0")));
    }

    #[test]
    fn failures_land_where_the_person_can_retry() {
        let failed_check = on_check_failure(&on_check_start(&idle(), AT), "offline", AT);
        assert_eq!((failed_check.status, failed_check.error_context, failed_check.can_retry, failed_check.message.as_deref()), (Status::Error, Some(ErrorContext::Check), true, Some("offline")));
        let held = on_download_complete(&idle(), "1.1.0");
        let quiet = on_check_failure(&held, "offline", AT);
        assert_eq!((quiet.status, quiet.message), (Status::Downloaded, None), "a failed check does not lose a download");
        let found = on_update_available(&idle(), "1.1.0", AT, vec![], 0);
        let download_failed = on_download_failure(&found, "disk full");
        assert_eq!((download_failed.status, download_failed.can_retry, download_failed.error_context), (Status::Available, true, Some(ErrorContext::Download)));
        assert_eq!(on_download_failure(&idle(), "x").status, Status::Error);
        let install_failed = on_install_failure(&held, "locked");
        assert_eq!((install_failed.status, install_failed.error_context), (Status::Downloaded, Some(ErrorContext::Install)));
    }

    #[test]
    fn the_same_version_found_again_stays_downloaded() {
        let held = on_download_complete(&on_update_available(&idle(), "1.1.0", AT, vec![ReleaseNote { version: "1.1.0".into(), items: vec![], total_items: 0 }], 2), "1.1.0");
        let again = on_update_available(&held, "1.1.0", AT, vec![], 0);
        assert_eq!((again.status, again.release_notes.len(), again.omitted_release_count), (Status::Downloaded, 1, 2), "the notes already held are kept");
        let newer = on_update_available(&held, "1.2.0", AT, vec![], 0);
        assert_eq!((newer.status, newer.downloaded_version), (Status::Available, None));
    }

    #[test]
    fn a_run_the_server_asked_for_checks_downloads_and_installs_within_its_caps() {
        let none = Attempts::default();
        assert_eq!(next_step(&idle(), none, None), Step::Check);
        assert_eq!(next_step(&idle(), Attempts { checks: 2, downloads: 0 }, None), Step::Done { outcome: Outcome::Failed, reason: Some("The desktop app did not report an update result.".into()) });
        // Retained results are stale before the run has looked itself.
        let up_to_date = on_no_update(&idle(), AT);
        assert_eq!(next_step(&up_to_date, none, None), Step::Check);
        assert_eq!(next_step(&up_to_date, Attempts { checks: 1, downloads: 0 }, None), Step::Done { outcome: Outcome::UpToDate, reason: None });
        let found = on_update_available(&idle(), "1.1.0", AT, vec![], 0);
        assert_eq!(next_step(&found, none, None), Step::Download);
        assert!(matches!(next_step(&found, Attempts { checks: 1, downloads: 3 }, None), Step::Done { outcome: Outcome::Failed, .. }));
        assert_eq!(next_step(&on_download_start(&found), none, None), Step::Wait);
        assert_eq!(next_step(&on_download_complete(&found, "1.1.0"), none, None), Step::Install);
        // A download survives an unrelated error and a failed install, not a failed download.
        let mut after_install = on_install_failure(&on_download_complete(&found, "1.1.0"), "locked");
        after_install.status = Status::Error;
        assert_eq!(next_step(&after_install, none, None), Step::Install);
        after_install.error_context = Some(ErrorContext::Download);
        assert_eq!(next_step(&after_install, none, None), Step::Check);
    }

    #[test]
    fn with_no_feed_a_request_is_answered_as_the_original_answers_it() {
        let state = unavailable("1.0.0", &runtime(), Channel::Latest);
        let reason = disabled_reason(&Environment { has_update_feed_config: false, is_development: false, is_packaged: true, disabled_by_env: false });
        assert_eq!(reason, Some("Automatic updates are not available because no update feed is configured."));
        let frame = unavailable_report("request-1", &state, reason);
        assert_eq!(frame["type"], "desktopUpdateStatus");
        assert_eq!((frame["version"].clone(), frame["requestId"].clone(), frame["outcome"].clone()), (json!(1), json!("request-1"), json!("failed")));
        assert_eq!(frame["reason"], "Automatic updates are not available because no update feed is configured.");
        assert_eq!((frame["state"]["enabled"].clone(), frame["state"]["status"].clone(), frame["state"]["channel"].clone(), frame["state"]["currentVersion"].clone()), (json!(false), json!("disabled"), json!("latest"), json!("1.0.0")));
        assert_eq!(frame["state"]["downloadPercent"], Value::Null, "absent values are sent as null, as the original's schema has them");
        // Without a stated reason the original's own words are used.
        assert_eq!(unavailable_report("r", &state, None)["reason"], "Automatic updates are disabled on this machine.");
    }

    #[test]
    fn the_other_reasons_updating_cannot_happen_are_told_in_order() {
        let reason = |feed, development, packaged, by_env| disabled_reason(&Environment { has_update_feed_config: feed, is_development: development, is_packaged: packaged, disabled_by_env: by_env });
        assert!(reason(true, true, true, true).unwrap().contains("packaged production builds"));
        assert!(reason(true, false, true, true).unwrap().contains("T3CODE_DISABLE_AUTO_UPDATE"));
        assert_eq!(reason(true, false, true, false), None);
    }

    #[test]
    fn a_blank_reason_is_dropped_so_the_report_still_encodes() {
        assert_eq!(normalize_reason(Some("  \n")), None);
        assert_eq!(normalize_reason(Some(" why ")), Some("why".into()));
        let frame = report(&idle(), Some("r"), Some((Outcome::Failed, Some("   "))));
        assert!(frame.get("reason").is_none());
        assert_eq!(not_prepared_report("r", &idle())["reason"], "This desktop update is no longer prepared.");
    }
}

// ---- What the window shows of the state (`desktopUpdate.logic.ts`) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonAction {
    Download,
    Install,
    None,
}

/// `resolveDesktopUpdateButtonAction`.
pub fn button_action(state: &State) -> ButtonAction {
    if installable(state) {
        return ButtonAction::Install;
    }
    match (state.status, state.error_context, &state.available_version) {
        (Status::Available, _, _) | (Status::Error, Some(ErrorContext::Download), Some(_)) => ButtonAction::Download,
        _ => ButtonAction::None,
    }
}

/// `canCheckForUpdate`.
pub fn can_check(state: &State) -> bool {
    state.enabled && !matches!(state.status, Status::Checking | Status::Downloading | Status::Disabled)
}

/// `getDesktopUpdateButtonTooltip`.
pub fn button_tooltip(state: &State) -> String {
    match state.status {
        Status::Available => format!("Update {} ready to download", state.available_version.as_deref().unwrap_or("available")),
        Status::Downloading => format!("Downloading update{}", state.download_percent.map(|percent| format!(" ({}%)", percent.floor())).unwrap_or_default()),
        Status::Downloaded => format!("Update {} downloaded. Click to restart and install.", state.downloaded_version.as_deref().or(state.available_version.as_deref()).unwrap_or("ready")),
        Status::Error => match (state.error_context, &state.available_version, &state.downloaded_version) {
            (Some(ErrorContext::Download), Some(version), _) => format!("Download failed for {version}. Click to retry."),
            (Some(ErrorContext::Install), _, Some(version)) => format!("Install failed for {version}. Click to retry."),
            (_, _, Some(version)) => format!("Update {version} downloaded. Click to restart and install."),
            _ => state.message.clone().unwrap_or_else(|| "Update failed".into()),
        },
        _ => "Up to date".into(),
    }
}

/// Whether the sidebar's update pill is drawn: an update to download, one downloading or downloaded, or a failure to say.
/// Disabled, idle, checking and up to date draw nothing.
pub fn shows_pill(state: &State) -> bool {
    button_action(state) != ButtonAction::None || matches!(state.status, Status::Downloading | Status::Error)
}

/// `shouldShowArm64IntelBuildWarning`.
pub fn arm64_intel_build(state: &State) -> bool {
    state.host_arch == "arm64" && state.app_arch == "x64"
}

// ---- The choice of channel (`DesktopAppSettings.ts`) ----

/// The file the choice is kept in, beside the app's other files. It is this app's own: the original's settings file is not touched.
pub const SETTINGS_FILE: &str = "desktop-settings.json";

/// The channel the person chose, if they did (`updateChannelConfiguredByUser`); none keeps the version's default.
pub fn read_channel(path: &std::path::Path) -> Option<Channel> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if value["updateChannelConfiguredByUser"] != true {
        return None;
    }
    serde_json::from_value(value["updateChannel"].clone()).ok()
}
pub fn write_channel(path: &std::path::Path, channel: Channel) -> std::io::Result<()> {
    // The document also holds the other settings (`settings`): they stay as they are.
    crate::settings::edit(path, |document| {
        document.insert("updateChannel".into(), json!(channel));
        document.insert("updateChannelConfiguredByUser".into(), json!(true));
    })
}
/// The channel in force: the choice, else the version's default.
pub fn channel_in_force(path: &std::path::Path, app_version: &str) -> Channel {
    read_channel(path).unwrap_or_else(|| default_channel(app_version))
}

#[cfg(test)]
mod window_tests {
    use super::*;

    fn runtime() -> RuntimeInfo {
        RuntimeInfo { host_arch: "arm64".into(), app_arch: "x64".into(), running_under_arm64_translation: true }
    }

    #[test]
    fn the_pill_and_the_check_button_follow_the_state() {
        let disabled = initial("1.0.0", &runtime(), Channel::Latest);
        assert!(!shows_pill(&disabled) && !can_check(&disabled));
        let idle = State { enabled: true, status: Status::Idle, ..disabled.clone() };
        assert!(!shows_pill(&idle) && can_check(&idle));
        let found = on_update_available(&idle, "1.1.0", "t", vec![], 0);
        assert_eq!((button_action(&found), shows_pill(&found), button_tooltip(&found)), (ButtonAction::Download, true, "Update 1.1.0 ready to download".into()));
        let progress = on_download_progress(&on_download_start(&found), 42.7);
        assert_eq!((shows_pill(&progress), button_tooltip(&progress)), (true, "Downloading update (42%)".into()));
        let done = on_download_complete(&progress, "1.1.0");
        assert_eq!((button_action(&done), button_tooltip(&done)), (ButtonAction::Install, "Update 1.1.0 downloaded. Click to restart and install.".into()));
        let failed = on_download_failure(&on_update_available(&idle, "1.1.0", "t", vec![], 0), "disk full");
        assert_eq!(failed.status, Status::Available);
        let error = State { status: Status::Error, ..failed };
        assert_eq!((button_action(&error), button_tooltip(&error)), (ButtonAction::Download, "Download failed for 1.1.0. Click to retry.".into()));
        assert_eq!(button_tooltip(&idle), "Up to date");
        assert!(arm64_intel_build(&disabled));
    }

    #[test]
    fn a_chosen_channel_is_kept_and_without_a_choice_the_version_decides() {
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-desktop-channel-file-").tempdir().unwrap();
        let path = directory.path().join(SETTINGS_FILE);
        assert_eq!(channel_in_force(&path, "1.2.3"), Channel::Latest);
        assert_eq!(channel_in_force(&path, "1.2.3-nightly.20260101.1"), Channel::Nightly);
        write_channel(&path, Channel::Nightly).unwrap();
        assert_eq!(channel_in_force(&path, "1.2.3"), Channel::Nightly);
        write_channel(&path, Channel::Latest).unwrap();
        assert_eq!(channel_in_force(&path, "1.2.3-nightly.20260101.1"), Channel::Latest, "a choice outranks the version");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(channel_in_force(&path, "1.2.3"), Channel::Latest, "a damaged file is no choice");
    }
}
