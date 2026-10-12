//! The app's own settings file (`DesktopAppSettings.ts`): whether the local environment runs, whether the server listens beyond this
//! machine, and Tailscale Serve. The update track (`updates::write_channel`) and any key this app does not know live in the same
//! document, so a change here reads the document, changes its keys and writes it back whole. As in the original, a value equal to its
//! default is left out of the file. The file is this app's own: the original's settings file is never touched.
use crate::network::Mode;
use serde_json::{Map, Value};
use std::path::Path;

pub const DEFAULT_TAILSCALE_SERVE_PORT: u16 = 443;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Whether this app starts the server that runs on this computer (`localEnvironmentEnabled`).
    pub local_environment_enabled: bool,
    pub server_exposure_mode: Mode,
    pub tailscale_serve_enabled: bool,
    pub tailscale_serve_port: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self { local_environment_enabled: true, server_exposure_mode: Mode::LocalOnly, tailscale_serve_enabled: false, tailscale_serve_port: DEFAULT_TAILSCALE_SERVE_PORT }
    }
}

/// `normalizeDesktopSettingsDocument` for the keys kept here: a value of the wrong type or out of range is the default.
pub fn parse(document: &Map<String, Value>) -> Settings {
    let defaults = Settings::default();
    Settings {
        local_environment_enabled: document.get("localEnvironmentEnabled").and_then(Value::as_bool).unwrap_or(defaults.local_environment_enabled),
        server_exposure_mode: document.get("serverExposureMode").and_then(Value::as_str).map_or(defaults.server_exposure_mode, Mode::from_wire),
        tailscale_serve_enabled: document.get("tailscaleServeEnabled").and_then(Value::as_bool) == Some(true),
        tailscale_serve_port: document.get("tailscaleServePort").and_then(Value::as_u64).and_then(|port| u16::try_from(port).ok()).filter(|port| *port >= 1).unwrap_or(defaults.tailscale_serve_port),
    }
}

/// The document in the file; empty when there is none or it is not an object.
fn read_document(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path).ok().and_then(|text| serde_json::from_str::<Value>(&text).ok()).and_then(|value| if let Value::Object(map) = value { Some(map) } else { None }).unwrap_or_default()
}

pub fn load(path: &Path) -> Settings {
    parse(&read_document(path))
}

/// Writes the document whole or not at all, so a crash mid-write leaves the earlier file.
fn write_document(path: &Path, document: &Map<String, Value>) -> std::io::Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(document).expect("a settings document is plain data"))?;
    std::fs::rename(temporary, path)
}

/// Reads the document, lets `change` edit it, and writes it back with every other key as it was.
pub(crate) fn edit(path: &Path, change: impl FnOnce(&mut Map<String, Value>)) -> std::io::Result<()> {
    let mut document = read_document(path);
    change(&mut document);
    write_document(path, &document)
}

/// `toDesktopSettingsDocument`: a key is kept when its value is not the default.
fn put(document: &mut Map<String, Value>, settings: &Settings) {
    let defaults = Settings::default();
    let mut set = |key: &str, value: Value, is_default: bool| {
        if is_default {
            document.remove(key);
        } else {
            document.insert(key.to_owned(), value);
        }
    };
    set("localEnvironmentEnabled", Value::Bool(settings.local_environment_enabled), settings.local_environment_enabled == defaults.local_environment_enabled);
    set("serverExposureMode", Value::from(settings.server_exposure_mode.wire()), settings.server_exposure_mode == defaults.server_exposure_mode);
    set("tailscaleServeEnabled", Value::Bool(settings.tailscale_serve_enabled), settings.tailscale_serve_enabled == defaults.tailscale_serve_enabled);
    set("tailscaleServePort", Value::from(settings.tailscale_serve_port), settings.tailscale_serve_port == defaults.tailscale_serve_port);
}

/// What a change did: the settings now, and whether any of them is not what it was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub settings: Settings,
    pub changed: bool,
}

/// Applies `change` to the saved settings and saves them. Nothing is written when nothing changed.
pub fn update(path: &Path, change: impl FnOnce(&mut Settings)) -> std::io::Result<Change> {
    let before = load(path);
    let mut settings = before.clone();
    change(&mut settings);
    let changed = settings != before;
    if changed {
        edit(path, |document| put(document, &settings))?;
    }
    Ok(Change { settings, changed })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-desktop-settings-").tempdir().unwrap();
        let path = directory.path().join("desktop-settings.json");
        (directory, path)
    }

    #[test]
    fn without_a_file_every_setting_is_its_default_and_a_wrong_value_is_too() {
        let (_directory, path) = file();
        assert_eq!(load(&path), Settings::default());
        std::fs::write(&path, r#"{"serverExposureMode":"everywhere","tailscaleServePort":70000,"tailscaleServeEnabled":"yes","localEnvironmentEnabled":0}"#).unwrap();
        assert_eq!(load(&path), Settings::default());
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load(&path), Settings::default());
    }

    #[test]
    fn a_change_is_kept_and_a_key_this_app_does_not_know_survives_it() {
        let (_directory, path) = file();
        std::fs::write(&path, r#"{"updateChannel":"nightly","updateChannelConfiguredByUser":true,"wslOnly":true}"#).unwrap();
        let change = update(&path, |settings| {
            settings.server_exposure_mode = Mode::NetworkAccessible;
            settings.tailscale_serve_port = 8443;
        })
        .unwrap();
        assert!(change.changed);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["serverExposureMode"], "network-accessible");
        assert_eq!(saved["tailscaleServePort"], 8443);
        assert_eq!((saved["updateChannel"].as_str(), saved["wslOnly"].as_bool()), (Some("nightly"), Some(true)));
        assert_eq!(load(&path), change.settings);
    }

    #[test]
    fn a_value_back_at_its_default_leaves_the_file_and_nothing_is_written_when_nothing_changed() {
        let (_directory, path) = file();
        update(&path, |settings| settings.local_environment_enabled = false).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("localEnvironmentEnabled"));
        let unchanged = update(&path, |settings| settings.local_environment_enabled = false).unwrap();
        assert!(!unchanged.changed);
        update(&path, |settings| settings.local_environment_enabled = true).unwrap();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.get("localEnvironmentEnabled").is_none());
        let (_other, absent) = file();
        let nothing = update(&absent, |_| {}).unwrap();
        assert!(!nothing.changed && !absent.exists(), "no file is made for no change");
    }
}
