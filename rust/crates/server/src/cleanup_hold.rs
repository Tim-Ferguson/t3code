//! RUST-PORT EXTENSION: cleanup rules the original's settings carry are not acted on by this server until the user touches them.
//!
//! The original's `storageCleanup` and `worktreeCleanup` settings delete real worktrees, browser artifacts and rotated logs on a
//! timer. A user who moves to this server brings those settings along (`state_import_files::copy`), and the first start would
//! then run a new implementation of that deletion against the user's real checkouts. So when the settings an import carries
//! enable any rule, the import writes `rust-cleanup-hold.json` beside the store, and the cleanup reads the settings through
//! [`HeldSettings`]: while the cleanup rules are as the first start found them, it sees every rule switched off (the stored
//! values are untouched, so nothing is lost and nothing is rewritten in a settings file the original may still use). The first
//! change to any cleanup rule, environment-wide or of a project, ends the hold for good: what the user sets is acted on as
//! written, including a rule set back to the value it had.
use crate::storage_cleanup::{SettingsFeed, WorktreeRules, any_worktree_policy};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const HOLD_FILE: &str = "rust-cleanup-hold.json";

/// The rules of a settings document that the cleanup acts on, in the form the server's own snapshot has them.
fn rules_of(settings: &Value) -> Value {
    let projects: serde_json::Map<String, Value> = settings["projectSettingsOverrides"].as_object().into_iter().flatten().filter_map(|(id, overrides)| overrides.get("worktreeCleanup").map(|policy| (id.clone(), policy.clone()))).collect();
    json!({"storageCleanup": settings["storageCleanup"], "worktreeCleanup": settings["worktreeCleanup"], "projects": projects})
}

/// Whether any rule of the settings would delete something: a worktree rule of the environment or a project, or an age for
/// browser artifacts or logs.
pub fn enables_deletion(settings: &Value) -> bool {
    any_worktree_policy(settings, WorktreeRules::enabled) || ["browserArtifactsAfterDays", "logsAfterDays"].iter().any(|key| settings["storageCleanup"][key].as_f64().is_some())
}

/// A line for the import's report that lists what was switched off and how to switch it on.
fn describe(settings: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let cleanup = &settings["storageCleanup"];
    for (key, label) in [("worktreeAfterDays", "worktrees idle for days"), ("browserArtifactsAfterDays", "browser artifacts after days"), ("logsAfterDays", "logs after days")] {
        if let Some(days) = cleanup[key].as_f64() {
            parts.push(format!("{label}: {days}"));
        }
    }
    for (key, label) in [("worktreeOnMerge", "worktrees of merged branches"), ("worktreeOnDelete", "worktrees of deleted threads"), ("worktreeUnchanged", "unchanged worktrees")] {
        if cleanup[key] == true {
            parts.push(label.to_owned());
        }
    }
    if settings["worktreeCleanup"]["mode"] == "custom" {
        parts.push("an environment-wide worktree policy".to_owned());
    }
    let projects = settings["projectSettingsOverrides"].as_object().into_iter().flatten().filter(|(_, overrides)| overrides["worktreeCleanup"]["mode"] == "custom").count();
    if projects > 0 {
        parts.push(format!("custom worktree policies of {projects} project(s)"));
    }
    format!("Storage and worktree cleanup rules were carried over switched off ({}) because this server's deletion is new: they delete real checkouts. Their values are kept; change any cleanup rule in Settings (a rule set again to its value counts) to switch cleanup on as you set it", parts.join(", "))
}

/// Called by the import once the settings are in the state directory: holds the cleanup when they enable any deletion. The line
/// for the report is returned.
pub fn on_import(state_dir: &Path) -> Option<String> {
    let bytes = std::fs::read(state_dir.join("settings.json")).ok()?;
    let settings = crate::server_settings::decode_jsonc(&bytes).ok()?;
    // Settings that fail to decode are replaced by defaults at load, which delete nothing.
    if !settings.is_object() || !enables_deletion(&settings) {
        return None;
    }
    let held = json!({"heldBecause": "the import carried cleanup rules", "rules": null});
    std::fs::write(state_dir.join(HOLD_FILE), serde_json::to_vec(&held).ok()?).ok()?;
    Some(describe(&settings))
}

/// The cleanup's view of the settings: the server's, with the hold applied.
pub struct HeldSettings<S> {
    pub inner: S,
    pub state_dir: PathBuf,
}

impl<S> HeldSettings<S> {
    /// `settings` as the cleanup is to see them. A held file records the rules as the first snapshot found them; while they are
    /// unchanged every rule reads as off, and the first difference removes the file.
    fn view(&self, mut settings: Value) -> Value {
        let path = self.state_dir.join(HOLD_FILE);
        // Only a missing file means there is no hold: one that cannot be read or parsed still holds, since the other way round
        // a damaged file would switch deletion on.
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return settings,
            Err(_) => Vec::new(),
        };
        let mut hold = serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object).unwrap_or_else(|| json!({"rules": null}));
        let rules = rules_of(&settings);
        if hold["rules"].is_null() {
            hold["rules"] = rules.clone();
            let _ = std::fs::write(&path, serde_json::to_vec(&hold).unwrap_or_default());
        }
        if hold["rules"] != rules {
            let _ = std::fs::remove_file(&path);
            return settings;
        }
        settings["storageCleanup"] = json!({"worktreeAfterDays": null, "worktreeOnMerge": false, "worktreeOnDelete": false, "worktreeUnchanged": false, "browserArtifactsAfterDays": null, "logsAfterDays": null});
        settings["worktreeCleanup"] = json!({"mode": "off"});
        for overrides in settings["projectSettingsOverrides"].as_object_mut().into_iter().flatten().map(|(_, overrides)| overrides) {
            if let Some(object) = overrides.as_object_mut().filter(|object| object.contains_key("worktreeCleanup")) {
                object.insert("worktreeCleanup".into(), json!({"mode": "off"}));
            }
        }
        settings
    }
}

#[async_trait]
impl<S: SettingsFeed> SettingsFeed for HeldSettings<S> {
    async fn current(&self) -> Result<Value, String> {
        Ok(self.view(self.inner.current().await?))
    }
    async fn changes(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>> {
        let mut inner = self.inner.changes().await?;
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let held = HeldSettings { inner: (), state_dir: self.state_dir.clone() };
        tokio::spawn(async move {
            while let Some(next) = inner.recv().await {
                if sender.send(held.view(next)).is_err() {
                    break;
                }
            }
        });
        Some(receiver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_cleanup::resolve_worktree_cleanup;

    struct Fixed(std::sync::Mutex<Value>);
    #[async_trait]
    impl SettingsFeed for Fixed {
        async fn current(&self) -> Result<Value, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn changes(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Value>> {
            None
        }
    }

    fn settings(days: Option<u64>, on_delete: bool) -> Value {
        json!({"storageCleanup": {"worktreeAfterDays": days, "worktreeOnMerge": false, "worktreeOnDelete": on_delete, "worktreeUnchanged": false, "browserArtifactsAfterDays": null, "logsAfterDays": null}, "worktreeCleanup": null, "projectSettingsOverrides": {"p": {"worktreeCleanup": {"mode": "custom", "rules": {"worktreeAfterDays": 3, "worktreeOnMerge": true, "worktreeOnDelete": false, "worktreeUnchanged": false}}}}})
    }

    #[test]
    fn only_settings_that_would_delete_something_are_held() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("settings.json"), br#"{"storageCleanup":{"worktreeAfterDays":null,"logsAfterDays":null}}"#).unwrap();
        assert!(on_import(directory.path()).is_none());
        assert!(!directory.path().join(HOLD_FILE).exists());
        std::fs::write(directory.path().join("settings.json"), br#"{/* kept */"storageCleanup":{"worktreeAfterDays":14,"worktreeOnDelete":true,},}"#).unwrap();
        let line = on_import(directory.path()).expect("an enabled rule is held");
        assert!(line.contains("switched off") && line.contains("14") && line.contains("deleted threads"), "{line}");
        assert!(directory.path().join(HOLD_FILE).exists());
        std::fs::write(directory.path().join("settings.json"), br#"{"projectSettingsOverrides":{"p":{"worktreeCleanup":{"mode":"custom","rules":{"worktreeAfterDays":null,"worktreeOnMerge":true,"worktreeOnDelete":false,"worktreeUnchanged":false}}}}}"#).unwrap();
        assert!(on_import(directory.path()).is_some(), "a project's own rule counts");
        std::fs::write(directory.path().join("settings.json"), br#"{"storageCleanup":{"logsAfterDays":30}}"#).unwrap();
        assert!(on_import(directory.path()).is_some(), "so does a log age");
    }

    #[tokio::test]
    async fn rules_read_as_off_until_the_user_changes_one_and_their_values_are_kept() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(HOLD_FILE), br#"{"rules":null}"#).unwrap();
        let held = HeldSettings { inner: Fixed(std::sync::Mutex::new(settings(Some(7), true))), state_dir: directory.path().to_owned() };
        let seen = held.current().await.unwrap();
        for project in [None, Some("p"), Some("other")] {
            assert!(!resolve_worktree_cleanup(&seen, project).enabled(), "{project:?}");
        }
        assert!(!enables_deletion(&seen));
        // The stored values are not touched: the held file recorded them, the server's settings still hold 7 days.
        assert_eq!(held.inner.current().await.unwrap()["storageCleanup"]["worktreeAfterDays"], 7);
        assert!(directory.path().join(HOLD_FILE).exists());
        // Unrelated changes (and the same rules read again) leave the hold.
        let mut unrelated = settings(Some(7), true);
        unrelated["responseStreamingMode"] = json!("turn");
        *held.inner.0.lock().unwrap() = unrelated;
        assert!(!enables_deletion(&held.current().await.unwrap()));
        // A change to a cleanup rule ends it, for good, and is acted on as written.
        *held.inner.0.lock().unwrap() = settings(Some(30), true);
        let lifted = held.current().await.unwrap();
        assert!(resolve_worktree_cleanup(&lifted, None).enabled());
        assert!(!directory.path().join(HOLD_FILE).exists());
        *held.inner.0.lock().unwrap() = settings(Some(7), true);
        assert!(enables_deletion(&held.current().await.unwrap()), "once lifted, the old values are the user's again");
    }

    #[tokio::test]
    async fn a_damaged_hold_file_still_holds() {
        for damaged in [&b"{\"rules\""[..], b"", b"[]"] {
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join(HOLD_FILE), damaged).unwrap();
            let held = HeldSettings { inner: Fixed(std::sync::Mutex::new(settings(Some(7), true))), state_dir: directory.path().to_owned() };
            assert!(!enables_deletion(&held.current().await.unwrap()), "{damaged:?}");
            assert!(directory.path().join(HOLD_FILE).exists());
        }
    }

    #[tokio::test]
    async fn without_a_hold_file_the_settings_are_read_as_they_are() {
        let directory = tempfile::tempdir().unwrap();
        let held = HeldSettings { inner: Fixed(std::sync::Mutex::new(settings(Some(7), true))), state_dir: directory.path().to_owned() };
        assert_eq!(held.current().await.unwrap(), settings(Some(7), true));
    }
}
