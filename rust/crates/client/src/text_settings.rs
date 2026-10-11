//! The text generation settings: which provider instances may write text, where a setting's
//! effective value comes from (the environment, or a project that overrides it), and the patches
//! that change it. Port of `SourceControlWritingSettings.tsx`, `BranchNamingSettings.tsx`, the
//! scoped-settings planning of `scopedSettings.ts` and the Git fetch row of
//! `SourceControlSettings.tsx`. Everything reads the wire JSON of the server settings.
use serde_json::{Map, Value, json};

/// The drivers that can write text. ACP Registry instances and the other drivers cannot.
pub const TEXT_GENERATION_DRIVERS: [&str; 2] = ["codex", "claudeAgent"];

/// A model of an instance that may write text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub instance_id: String,
    pub instance_label: String,
    pub model: String,
    pub model_label: String,
}
impl Choice {
    /// What a select's value carries: the pair, so two instances with a model of one name differ.
    pub fn key(&self) -> String {
        json!([self.instance_id, self.model]).to_string()
    }
    pub fn selection(&self) -> Value {
        json!({"instanceId":self.instance_id,"model":self.model})
    }
    pub fn label(&self) -> String {
        format!("{} \u{b7} {}", self.instance_label, self.model_label)
    }
}
pub fn key_of(selection: &Value) -> String {
    json!([selection["instanceId"], selection["model"]]).to_string()
}

/// The models offered: those of enabled, available instances whose driver is `codex` or
/// `claudeAgent` and whose snapshot does not say it cannot write text.
pub fn choices(providers: &Value) -> Vec<Choice> {
    let mut rows = Vec::new();
    for provider in providers.as_array().into_iter().flatten() {
        let driver = provider["driver"].as_str().unwrap_or_default();
        if !TEXT_GENERATION_DRIVERS.contains(&driver) || provider["supportsTextGeneration"] == false || provider["enabled"] == false || provider["availability"] == "unavailable" {
            continue;
        }
        let Some(instance_id) = provider["instanceId"].as_str() else { continue };
        let label = provider["displayName"].as_str().filter(|name| !name.is_empty()).unwrap_or(instance_id);
        for model in provider["models"].as_array().into_iter().flatten() {
            let Some(slug) = model["slug"].as_str() else { continue };
            rows.push(Choice { instance_id: instance_id.into(), instance_label: label.into(), model: slug.into(), model_label: model["name"].as_str().unwrap_or(slug).into() });
        }
    }
    rows
}

/// Which setting a page row changes. Every row of the page is one of these.
pub const TEXT_GENERATION_MODEL: &str = "textGenerationModelSelection";
pub const WRITER_MODEL: &str = "sourceControlWriterModelSelection";
pub const WRITING_STYLE: &str = "sourceControlWritingStyle";
pub const BRANCH_MODE: &str = "branchNamingMode";
pub const BRANCH_PREFIX: &str = "branchNamePrefix";
pub const BRANCH_INSTRUCTIONS: &str = "branchNameInstructions";

/// A project's override entry, when it has one.
fn overrides<'a>(settings: &'a Value, project: Option<&str>) -> Option<&'a Value> {
    project.map(|project| &settings["projectSettingsOverrides"][project]).filter(|entry| entry.is_object())
}
/// Whether the project overrides the setting (an override of null counts, for the settings that
/// can be null).
pub fn is_overridden(settings: &Value, project: Option<&str>, key: &str) -> bool {
    overrides(settings, project).is_some_and(|entry| entry.get(key).is_some())
}
/// The value in effect: the project's override where it has one, else the environment's.
pub fn effective(settings: &Value, project: Option<&str>, key: &str) -> Value {
    overrides(settings, project).and_then(|entry| entry.get(key)).cloned().unwrap_or_else(|| settings[key].clone())
}

/// The patch that sets one setting for the environment, or for one project's override entry.
/// An object value (the writing style) is merged into the effective one, as an override stores
/// the whole value.
pub fn set_patch(settings: &Value, project: Option<&str>, key: &str, value: Value) -> Value {
    match project {
        None => json!({"patch":{key:value}}),
        Some(project) => {
            let mut entry = overrides(settings, Some(project)).and_then(Value::as_object).cloned().unwrap_or_default();
            let base = effective(settings, Some(project), key);
            let merged = match (&base, &value) {
                (Value::Object(base), Value::Object(change)) => Value::Object(base.iter().chain(change.iter()).map(|(k, v)| (k.clone(), v.clone())).collect::<Map<_, _>>()),
                _ => value,
            };
            entry.insert(key.into(), merged);
            json!({"patch":{"projectSettingsOverrides":{project:entry}}})
        }
    }
}
/// The patch that makes a project inherit a setting again. `None` where there is nothing to clear.
pub fn clear_patch(settings: &Value, project: Option<&str>, key: &str) -> Option<Value> {
    let project = project?;
    let mut entry = overrides(settings, Some(project))?.as_object()?.clone();
    entry.remove(key)?;
    // An entry with nothing left is removed, which the server reads as null.
    Some(json!({"patch":{"projectSettingsOverrides":{project:if entry.is_empty() { Value::Null } else { Value::Object(entry) }}}}))
}

/// The model the source control writer uses: its own where set, else the text generation model.
pub fn writer_model(settings: &Value, project: Option<&str>) -> Value {
    let own = effective(settings, project, WRITER_MODEL);
    if own.is_object() { own } else { effective(settings, project, TEXT_GENERATION_MODEL) }
}

pub const WRITING_MODES: [(&str, &str, &str); 3] = [
    ("repo_conventions", "Repository conventions", "In each project, matches recent change descriptions and change request titles."),
    ("conventional_commits", "Conventional Commits", "Use Conventional Commit prefixes and keep change request text concise."),
    ("custom", "Custom instructions", "Use your instructions for change descriptions and change requests in every project."),
];
pub const BRANCH_MODES: [(&str, &str); 3] = [("static", "Static prefix"), ("semantic", "Semantic prefix"), ("custom", "Custom instructions")];
pub const SEMANTIC_NOTE: &str = "The model chooses a prefix that describes the work, such as feat/add-search, fix/login-timeout, or refactor/auth.";
pub const PREFIX_NOTE: &str = "For example, t3 or t3/ produces t3/add-search. Leave empty for no prefix.";
pub const INSTRUCTIONS_NOTE: &str = "Appended to the naming prompt. The model returns the complete branch name; no prefix or suffix is added.";
pub const WRITER_NOTE: &str = "Model for source control text and branch or bookmark names. Off uses the environment's text generation model.";
pub const TEMPLATES_NOTE: &str = "Use the repository's template for change request descriptions when available.";

/// The automatic Git fetch interval, which lives in the background activity policy: a preset's
/// value unless the policy overrides it.
pub const FETCH_STEP_SECONDS: u64 = 5;
fn preset_fetch_seconds(profile: &str) -> u64 {
    match profile {
        "performance" => 15,
        "battery-saver" => 0,
        _ => 30,
    }
}
fn base_profile(activity: &Value) -> &str {
    match activity["profile"].as_str() {
        Some("custom") => activity["baseProfile"].as_str().unwrap_or("balanced"),
        Some(profile) => profile,
        None => "balanced",
    }
}
pub fn preset_seconds(activity: &Value) -> u64 {
    preset_fetch_seconds(base_profile(activity))
}
pub fn fetch_seconds(activity: &Value) -> u64 {
    override_seconds(activity, "automaticGitFetchInterval", preset_seconds(activity))
}
/// The patch that sets the interval, or (with `None`) goes back to the preset's. Setting keeps
/// the other overrides and turns the policy into a custom one on the same base, as the original.
pub fn fetch_patch(activity: &Value, seconds: Option<u64>) -> Value {
    interval_patch(activity, "automaticGitFetchInterval", seconds)
}

/// The provider health check interval, which lives in the same policy as the fetch interval.
pub const HEALTH_STEP_SECONDS: u64 = 30;
fn preset_health_seconds(profile: &str) -> u64 {
    match profile {
        "performance" => 60,
        "battery-saver" => 900,
        _ => 300,
    }
}
pub fn health_preset_seconds(activity: &Value) -> u64 {
    preset_health_seconds(base_profile(activity))
}
pub fn health_seconds(activity: &Value) -> u64 {
    override_seconds(activity, "providerHealthRefreshInterval", health_preset_seconds(activity))
}
pub fn health_patch(activity: &Value, seconds: Option<u64>) -> Value {
    interval_patch(activity, "providerHealthRefreshInterval", seconds)
}

fn override_seconds(activity: &Value, key: &str, preset: u64) -> u64 {
    if activity["profile"] == "custom" {
        if let Some(millis) = activity["overrides"][key].as_u64() {
            return (millis as f64 / 1000.0).round() as u64;
        }
    }
    preset
}
fn interval_patch(activity: &Value, key: &str, seconds: Option<u64>) -> Value {
    let mut overrides = if activity["profile"] == "custom" { activity["overrides"].as_object().cloned().unwrap_or_default() } else { Map::new() };
    match seconds {
        Some(seconds) => {
            overrides.insert(key.into(), json!(seconds * 1000));
        }
        None => {
            overrides.remove(key);
        }
    }
    json!({"patch":{"backgroundActivity":{"schemaVersion":1,"profile":"custom","baseProfile":base_profile(activity),"overrides":overrides}}})
}
/// A typed interval: whole seconds, nothing for what is not a number, never below zero.
pub fn parse_seconds(text: &str) -> Option<u64> {
    let value: f64 = text.trim().parse().ok()?;
    value.is_finite().then(|| value.max(0.0).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> Value {
        json!([
            {"instanceId":"codex","driver":"codex","enabled":true,"models":[{"slug":"gpt","name":"GPT"},{"slug":"mini","name":"Mini"}]},
            {"instanceId":"claude","driver":"claudeAgent","displayName":"Claude","enabled":true,"supportsTextGeneration":true,"models":[{"slug":"opus","name":"Opus"}]},
            {"instanceId":"cursor","driver":"cursor","enabled":true,"models":[{"slug":"c","name":"C"}]},
            {"instanceId":"registry","driver":"acp","enabled":true,"models":[{"slug":"r","name":"R"}]},
            {"instanceId":"codex-2","driver":"codex","enabled":true,"supportsTextGeneration":false,"models":[{"slug":"x","name":"X"}]},
            {"instanceId":"codex-off","driver":"codex","enabled":false,"models":[{"slug":"y","name":"Y"}]},
            {"instanceId":"codex-gone","driver":"codex","enabled":true,"availability":"unavailable","models":[{"slug":"z","name":"Z"}]}
        ])
    }

    #[test]
    fn only_codex_and_claude_instances_that_can_write_text_are_offered() {
        let rows = choices(&providers());
        assert_eq!(rows.iter().map(|row| (row.instance_id.as_str(), row.model.as_str())).collect::<Vec<_>>(), [("codex", "gpt"), ("codex", "mini"), ("claude", "opus")]);
        assert_eq!(rows[2].label(), "Claude \u{b7} Opus");
        assert_eq!(key_of(&rows[0].selection()), rows[0].key());
    }

    #[test]
    fn a_project_override_wins_and_the_environment_is_the_default() {
        let settings = json!({"branchNamingMode":"static","sourceControlWriterModelSelection":null,"sourceControlWritingStyle":{"mode":"repo_conventions","customInstructions":"","followChangeRequestTemplates":true},
            "projectSettingsOverrides":{"p":{"branchNamingMode":"semantic","sourceControlWriterModelSelection":null}},"textGenerationModelSelection":{"instanceId":"codex","model":"gpt"}});
        assert_eq!(effective(&settings, Some("p"), BRANCH_MODE), json!("semantic"));
        assert_eq!(effective(&settings, Some("q"), BRANCH_MODE), json!("static"));
        assert_eq!(effective(&settings, None, BRANCH_MODE), json!("static"));
        assert!(is_overridden(&settings, Some("p"), WRITER_MODEL) && !is_overridden(&settings, Some("p"), BRANCH_PREFIX) && !is_overridden(&settings, None, BRANCH_MODE));
        assert_eq!(writer_model(&settings, Some("p")), json!({"instanceId":"codex","model":"gpt"}), "no writer of its own: the text generation model");
    }

    #[test]
    fn a_patch_sets_the_environment_or_the_whole_override_entry_of_a_project() {
        let settings = json!({"branchNamingMode":"static","sourceControlWritingStyle":{"mode":"repo_conventions","customInstructions":"keep","followChangeRequestTemplates":true},
            "projectSettingsOverrides":{"p":{"branchNamingMode":"semantic"}}});
        assert_eq!(set_patch(&settings, None, BRANCH_MODE, json!("custom")), json!({"patch":{"branchNamingMode":"custom"}}));
        assert_eq!(set_patch(&settings, Some("p"), BRANCH_PREFIX, json!("t3/")), json!({"patch":{"projectSettingsOverrides":{"p":{"branchNamingMode":"semantic","branchNamePrefix":"t3/"}}}}));
        // The writing style row sends one field; the override stores the whole style.
        assert_eq!(
            set_patch(&settings, Some("q"), WRITING_STYLE, json!({"mode":"custom"})),
            json!({"patch":{"projectSettingsOverrides":{"q":{"sourceControlWritingStyle":{"mode":"custom","customInstructions":"keep","followChangeRequestTemplates":true}}}}})
        );
        assert_eq!(clear_patch(&settings, Some("p"), BRANCH_MODE), Some(json!({"patch":{"projectSettingsOverrides":{"p":null}}})), "the last override takes the entry with it");
        assert_eq!(clear_patch(&settings, Some("p"), BRANCH_PREFIX), None);
        assert_eq!(clear_patch(&settings, None, BRANCH_MODE), None);
    }

    #[test]
    fn the_fetch_interval_is_the_presets_until_the_policy_overrides_it() {
        let balanced = json!({"schemaVersion":1,"profile":"balanced","overrides":{}});
        assert_eq!((fetch_seconds(&balanced), preset_seconds(&balanced)), (30, 30));
        assert_eq!(fetch_seconds(&json!({"profile":"performance","overrides":{}})), 15);
        assert_eq!(fetch_seconds(&json!({"profile":"battery-saver","overrides":{}})), 0);
        let custom = json!({"schemaVersion":1,"profile":"custom","baseProfile":"performance","overrides":{"automaticGitFetchInterval":45000,"idleClientTtl":1000}});
        assert_eq!((fetch_seconds(&custom), preset_seconds(&custom)), (45, 15));
        assert_eq!(
            fetch_patch(&custom, Some(60)),
            json!({"patch":{"backgroundActivity":{"schemaVersion":1,"profile":"custom","baseProfile":"performance","overrides":{"automaticGitFetchInterval":60000,"idleClientTtl":1000}}}})
        );
        assert_eq!(fetch_patch(&custom, None)["patch"]["backgroundActivity"]["overrides"], json!({"idleClientTtl":1000}));
        assert_eq!(fetch_patch(&balanced, Some(0))["patch"]["backgroundActivity"]["baseProfile"], "balanced");
        assert_eq!((parse_seconds(" 12.4 "), parse_seconds("-3"), parse_seconds("abc")), (Some(12), Some(0), None));
    }

    #[test]
    fn the_health_check_interval_follows_the_policy_like_the_fetch_interval() {
        // The presets of `backgroundActivitySettings.ts`: a minute, five minutes, a quarter of an hour.
        assert_eq!(health_seconds(&json!({"profile":"performance","overrides":{}})), 60);
        let balanced = json!({"schemaVersion":1,"profile":"balanced","overrides":{}});
        assert_eq!((health_seconds(&balanced), health_preset_seconds(&balanced)), (300, 300));
        assert_eq!(health_seconds(&json!({"profile":"battery-saver","overrides":{}})), 900);
        let custom = json!({"schemaVersion":1,"profile":"custom","baseProfile":"performance","overrides":{"automaticGitFetchInterval":45000,"providerHealthRefreshInterval":90000}});
        assert_eq!((health_seconds(&custom), health_preset_seconds(&custom), fetch_seconds(&custom)), (90, 60, 45));
        // Setting one interval leaves the other override alone; going back to the preset drops only its own.
        assert_eq!(health_patch(&custom, Some(120))["patch"]["backgroundActivity"]["overrides"], json!({"automaticGitFetchInterval":45000,"providerHealthRefreshInterval":120000}));
        assert_eq!(health_patch(&custom, None)["patch"]["backgroundActivity"]["overrides"], json!({"automaticGitFetchInterval":45000}));
        assert_eq!(health_patch(&balanced, Some(0))["patch"]["backgroundActivity"]["baseProfile"], "balanced");
    }
}

// ---- thread organization: limited threads and auto-settlement -----------------------------------

/// `autoResumeLimitedThreads` and `snoozeLimitedThreads` are environment settings; the two
/// auto-settle settings can also be overridden for one project.
pub const AUTO_RESUME_LIMITED: &str = "autoResumeLimitedThreads";
pub const SNOOZE_LIMITED: &str = "snoozeLimitedThreads";
pub const SETTLE_ON_MERGE: &str = "sidebarAutoSettleOnMerge";
pub const SETTLE_AFTER_DAYS: &str = "sidebarAutoSettleAfterDays";
/// What turning inactive-thread settling on starts at, and the range a day count may take
/// (`MIN_`/`MAX_SIDEBAR_AUTO_SETTLE_AFTER_DAYS`).
pub const SETTLE_DEFAULT_DAYS: u64 = 3;
pub const SETTLE_MIN_DAYS: u64 = 1;
pub const SETTLE_MAX_DAYS: u64 = 90;

/// A typed day count, only when it is a whole number in range: "3.5" is refused rather than
/// truncated to a 3 the field does not show (`AutoSettleDaysInput`).
pub fn parse_settle_days(text: &str) -> Option<u64> {
    let days: f64 = text.trim().parse().ok()?;
    (days.fract() == 0.0 && days >= SETTLE_MIN_DAYS as f64 && days <= SETTLE_MAX_DAYS as f64).then_some(days as u64)
}
/// The days a setting value stands for: none when settling by inactivity is off.
pub fn settle_days(value: &Value) -> Option<u64> {
    value.as_u64()
}

#[cfg(test)]
mod organization_tests {
    use super::*;

    #[test]
    fn a_day_count_is_a_whole_number_in_range() {
        assert_eq!(parse_settle_days("3"), Some(3));
        assert_eq!(parse_settle_days(" 90 "), Some(90));
        assert_eq!((parse_settle_days("0"), parse_settle_days("91"), parse_settle_days("3.5"), parse_settle_days(""), parse_settle_days("x")), (None, None, None, None, None));
    }

    #[test]
    fn the_settle_settings_set_and_inherit_per_project_and_never_is_a_value() {
        let settings = json!({SETTLE_AFTER_DAYS:3,"projectSettingsOverrides":{"p":{SETTLE_ON_MERGE:false}}});
        assert_eq!(set_patch(&settings, None, SETTLE_AFTER_DAYS, Value::Null), json!({"patch":{SETTLE_AFTER_DAYS:null}}));
        // For a project, null is the stored override that means "never settle", not "inherit".
        assert_eq!(set_patch(&settings, Some("p"), SETTLE_AFTER_DAYS, Value::Null), json!({"patch":{"projectSettingsOverrides":{"p":{SETTLE_ON_MERGE:false,SETTLE_AFTER_DAYS:null}}}}));
        assert!(is_overridden(&json!({"projectSettingsOverrides":{"p":{SETTLE_AFTER_DAYS:null}}}), Some("p"), SETTLE_AFTER_DAYS));
        assert_eq!(effective(&settings, Some("p"), SETTLE_AFTER_DAYS), json!(3));
        assert_eq!(clear_patch(&settings, Some("p"), SETTLE_ON_MERGE), Some(json!({"patch":{"projectSettingsOverrides":{"p":null}}})));
    }
}
