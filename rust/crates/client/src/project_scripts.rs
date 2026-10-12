//! Project actions ("scripts"): commands a project keeps to run in its checkout or worktree, with an icon, an optional shortcut
//! (`script.<id>.run`), and two roles: the one that sets up a new worktree and the one that runs when a thread settles. Port of
//! `projectScripts.ts` and `packages/shared/src/projectScripts.ts`. The list is the environment's `defaultProjectScripts`, or a
//! project's own override of it.
use crate::text_settings;
use serde_json::{Value, json};

pub const SETTING: &str = "defaultProjectScripts";
pub const MAX_ID_LENGTH: usize = 24;
pub const ICONS: [(&str, &str); 6] = [("play", "Play"), ("test", "Test"), ("lint", "Lint"), ("configure", "Configure"), ("build", "Build"), ("debug", "Debug")];
/// The text a script's icon is drawn as.
pub fn icon_glyph(icon: &str) -> &'static str {
    match icon {
        "test" => "\u{2713}",
        "lint" => "\u{2261}",
        "configure" => "\u{2699}",
        "build" => "\u{1f528}",
        "debug" => "\u{1f41b}",
        _ => "\u{25b6}",
    }
}

/// What the editor holds for a script (`NewProjectScriptInput`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub name: String,
    pub command: String,
    pub icon: String,
    pub run_on_worktree_create: bool,
    pub wait_for_setup: bool,
    pub run_on_settle: bool,
}
impl Default for Input {
    fn default() -> Self {
        Self { name: String::new(), command: String::new(), icon: "play".into(), run_on_worktree_create: false, wait_for_setup: false, run_on_settle: false }
    }
}
impl Input {
    pub fn of(script: &Value) -> Self {
        Self {
            name: script["name"].as_str().unwrap_or_default().to_owned(),
            command: script["command"].as_str().unwrap_or_default().to_owned(),
            icon: script["icon"].as_str().unwrap_or("play").to_owned(),
            run_on_worktree_create: script["runOnWorktreeCreate"] == true,
            wait_for_setup: script["runOnWorktreeCreate"] == true && script["async"] == false,
            run_on_settle: script["runOnSettle"] == true,
        }
    }
    /// Why the script cannot be saved, in the words the editor shows.
    pub fn problem(&self) -> Option<&'static str> {
        if self.name.trim().is_empty() {
            Some("Enter a name for the action.")
        } else if self.command.trim().is_empty() {
            Some("Enter the command the action runs.")
        } else {
            None
        }
    }
}

/// `buildProjectScript`. Preview fields are carried over from `previous` untouched (this editor has no controls for them).
pub fn build(id: &str, input: &Input, previous: Option<&Value>) -> Value {
    let mut script = json!({"id": id, "name": input.name.trim(), "command": input.command.trim(), "icon": input.icon, "runOnWorktreeCreate": input.run_on_worktree_create});
    if input.run_on_worktree_create && input.wait_for_setup {
        script["async"] = json!(false);
    }
    if input.run_on_settle {
        script["runOnSettle"] = json!(true);
    }
    if let Some(previous) = previous {
        for key in ["previewUrl", "autoOpenPreview"] {
            if !previous[key].is_null() {
                script[key] = previous[key].clone();
            }
        }
    }
    script
}

/// `releaseClaimedRoles`: a project runs at most one setup script and one settle script, so saving a script that claims either role
/// takes it from the script that held it.
pub fn release_claimed_roles(script: &Value, saved: &Input) -> Value {
    let release_setup = saved.run_on_worktree_create && script["runOnWorktreeCreate"] == true;
    let release_settle = saved.run_on_settle && script["runOnSettle"] == true;
    let mut script = script.clone();
    if release_setup {
        script["runOnWorktreeCreate"] = json!(false);
    }
    if release_settle {
        script["runOnSettle"] = json!(false);
    }
    script
}

fn normalize_id(name: &str) -> String {
    let mut cleaned = String::new();
    let mut dash = false;
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            cleaned.push(ch);
            dash = false;
        } else if !dash && !cleaned.is_empty() {
            cleaned.push('-');
            dash = true;
        }
    }
    let cleaned = cleaned.trim_matches('-').to_owned();
    if cleaned.is_empty() {
        return "script".into();
    }
    if cleaned.len() <= MAX_ID_LENGTH {
        return cleaned;
    }
    let cut = cleaned[..MAX_ID_LENGTH].trim_end_matches('-').to_owned();
    if cut.is_empty() { "script".into() } else { cut }
}

/// `nextProjectScriptId`: the name as an id, numbered when another script has it.
pub fn next_id(name: &str, existing: &[String]) -> String {
    let base = normalize_id(name);
    if !existing.contains(&base) {
        return base;
    }
    for suffix in 2..10_000 {
        let candidate = format!("{base}-{suffix}");
        let candidate = if candidate.len() <= MAX_ID_LENGTH {
            candidate
        } else {
            let keep = MAX_ID_LENGTH.saturating_sub(suffix.to_string().len() + 1).max(1);
            format!("{}-{suffix}", &base[..keep.min(base.len())])
        };
        if !existing.contains(&candidate) {
            return candidate;
        }
    }
    base
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_LENGTH && id.starts_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit()) && id.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}
/// `commandForProjectScript`: the keybinding command that runs a script; none for a legacy id that cannot carry a shortcut.
pub fn command_for(id: &str) -> Option<String> {
    valid_id(id).then(|| format!("script.{id}.run"))
}
/// `projectScriptIdFromCommand`.
pub fn id_from_command(command: &str) -> Option<&str> {
    let id = command.trim().strip_prefix("script.")?.strip_suffix(".run")?;
    valid_id(id).then_some(id)
}

/// `primaryProjectScript`: the one the header's Run button runs first.
pub fn primary(scripts: &[Value]) -> Option<&Value> {
    scripts.iter().find(|script| script["runOnWorktreeCreate"] != true && script["runOnSettle"] != true).or_else(|| scripts.iter().find(|script| script["runOnSettle"] != true))
}
/// `projectScriptMenuLabel`: the name with the roles it runs in.
pub fn menu_label(script: &Value) -> String {
    let name = script["name"].as_str().unwrap_or_default();
    let roles: Vec<&str> = [(script["runOnWorktreeCreate"] == true, "setup"), (script["runOnSettle"] == true, "on settle")].into_iter().filter_map(|(on, role)| on.then_some(role)).collect();
    if roles.is_empty() { name.to_owned() } else { format!("{name} ({})", roles.join(", ")) }
}

/// `resolveProjectScripts`: the project's override wins, then the environment's defaults. Until the legacy fields have been folded
/// into the override record, the old per-project map (null there meant "use the defaults") and the project's own scripts still count.
pub fn resolve(settings: &Value, project: Option<&str>, project_scripts: &[Value]) -> Vec<Value> {
    let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
    let Some(project) = project else { return list(&settings[SETTING]) };
    if let Some(own) = settings["projectSettingsOverrides"][project].get(SETTING).filter(|own| own.is_array()) {
        return list(own);
    }
    if settings["projectSettingsFolded"] == true {
        return list(&settings[SETTING]);
    }
    match settings["projectScriptOverrides"].get(project) {
        Some(Value::Null) => list(&settings[SETTING]),
        Some(legacy) => list(legacy),
        None if !project_scripts.is_empty() => project_scripts.to_vec(),
        None => list(&settings[SETTING]),
    }
}
/// Whether the project has no list of its own.
pub fn inherits(settings: &Value, project: Option<&str>, project_scripts: &[Value]) -> bool {
    let Some(project) = project else { return false };
    if settings["projectSettingsOverrides"][project].get(SETTING).is_some_and(Value::is_array) {
        return false;
    }
    if settings["projectSettingsFolded"] == true {
        return true;
    }
    match settings["projectScriptOverrides"].get(project) {
        Some(Value::Null) => true,
        Some(_) => false,
        None => project_scripts.is_empty(),
    }
}

/// The script a save leaves in the list: replaces the one with its id, else is added; the others give up a role it claims.
pub fn with_saved(current: &[Value], id: &str, input: &Input, existing: bool) -> Vec<Value> {
    let previous = current.iter().find(|script| script["id"] == id);
    let next = build(id, input, previous);
    let mut updated: Vec<Value> = current.iter().map(|script| if script["id"] == id { next.clone() } else { release_claimed_roles(script, input) }).collect();
    if !existing {
        updated.push(next);
    }
    updated
}

/// The patch that saves a list for the environment or a project (its override entry holds the whole list).
pub fn save_patch(settings: &Value, project: Option<&str>, scripts: Vec<Value>) -> Value {
    text_settings::set_patch(settings, project, SETTING, Value::Array(scripts))
}

/// `projectScriptRuntimeEnv`: what a script's terminal is told of where it runs.
pub fn runtime_env(project_root: &str, worktree: Option<&str>) -> Value {
    let mut env = json!({"T3CODE_PROJECT_ROOT": project_root});
    if let Some(worktree) = worktree.filter(|worktree| !worktree.is_empty()) {
        env["T3CODE_WORKTREE_PATH"] = json!(worktree);
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(id: &str, setup: bool, settle: bool) -> Value {
        json!({"id": id, "name": id, "command": format!("run {id}"), "icon": "play", "runOnWorktreeCreate": setup, "runOnSettle": settle})
    }

    #[test]
    fn a_name_becomes_an_id_that_is_numbered_when_taken_and_always_fits() {
        assert_eq!(next_id("Run dev server!", &[]), "run-dev-server");
        assert_eq!(next_id("  --  ", &[]), "script");
        assert_eq!(next_id("Test", &["test".into()]), "test-2");
        assert_eq!(next_id("Test", &["test".into(), "test-2".into()]), "test-3");
        let long = next_id("a very long name that goes past the limit of ids", &[]);
        assert!(long.len() <= MAX_ID_LENGTH && !long.ends_with('-'), "{long}");
        let taken = next_id("a very long name that goes past the limit of ids", &[long.clone()]);
        assert!(taken.len() <= MAX_ID_LENGTH && taken != long, "{taken}");
    }

    #[test]
    fn only_a_valid_id_has_a_shortcut_command_and_the_command_names_its_script() {
        assert_eq!(command_for("dev-server").as_deref(), Some("script.dev-server.run"));
        assert_eq!(command_for("Dev"), None);
        assert_eq!(command_for("-x"), None);
        assert_eq!(command_for(&"a".repeat(25)), None);
        assert_eq!(id_from_command("script.dev-server.run"), Some("dev-server"));
        assert_eq!(id_from_command(" script.x.run "), Some("x"));
        assert_eq!(id_from_command("script..run"), None);
        assert_eq!(id_from_command("diff.toggle"), None);
    }

    #[test]
    fn saving_a_script_that_takes_a_role_takes_it_from_the_one_that_had_it() {
        let current = vec![script("setup", true, false), script("clean", false, true), script("dev", false, false)];
        let input = Input { name: "Install".into(), command: "npm i".into(), run_on_worktree_create: true, run_on_settle: true, wait_for_setup: true, ..Input::default() };
        let saved = with_saved(&current, "install", &input, false);
        assert_eq!(saved.len(), 4);
        assert_eq!(saved[0]["runOnWorktreeCreate"], false);
        assert_eq!(saved[1]["runOnSettle"], false);
        assert_eq!(saved[2], current[2]);
        let new = &saved[3];
        assert_eq!((new["id"].as_str(), new["runOnWorktreeCreate"].clone(), new["async"].clone(), new["runOnSettle"].clone()), (Some("install"), json!(true), json!(false), json!(true)));
        // Editing one in place keeps its place and its preview fields.
        let mut with_preview = script("dev", false, false);
        with_preview["previewUrl"] = json!("http://localhost:3000");
        with_preview["autoOpenPreview"] = json!(true);
        let edited = with_saved(&[with_preview], "dev", &Input { name: "Dev".into(), command: "npm run dev".into(), ..Input::default() }, true);
        assert_eq!((edited[0]["name"].as_str(), edited[0]["previewUrl"].as_str(), edited[0]["autoOpenPreview"].clone()), (Some("Dev"), Some("http://localhost:3000"), json!(true)));
    }

    #[test]
    fn waiting_for_setup_only_means_something_for_the_setup_script() {
        let waits = Input { name: "a".into(), command: "b".into(), run_on_worktree_create: false, wait_for_setup: true, ..Input::default() };
        assert!(build("a", &waits, None).get("async").is_none());
        assert_eq!(Input::of(&build("a", &Input { run_on_worktree_create: true, ..waits.clone() }, None)).wait_for_setup, true);
        assert!(Input::default().problem().is_some());
        assert!(Input { name: "x".into(), ..Input::default() }.problem().is_some());
        assert!(Input { name: "x".into(), command: "y".into(), ..Input::default() }.problem().is_none());
    }

    #[test]
    fn the_run_button_prefers_a_plain_action_and_the_menu_names_the_roles() {
        let scripts = vec![script("setup", true, false), script("clean", false, true), script("dev", false, false)];
        assert_eq!(primary(&scripts).unwrap()["id"], "dev");
        assert_eq!(primary(&scripts[..1]).unwrap()["id"], "setup");
        assert!(primary(&scripts[1..2]).is_none());
        assert_eq!(menu_label(&scripts[0]), "setup (setup)");
        assert_eq!(menu_label(&script("both", true, true)), "both (setup, on settle)");
        assert_eq!(menu_label(&scripts[2]), "dev");
    }

    #[test]
    fn a_projects_list_is_its_override_else_the_environments_else_what_older_servers_kept() {
        let defaults = vec![script("d", false, false)];
        let own = vec![script("p", false, false)];
        let legacy = vec![script("l", false, false)];
        let settings = json!({"defaultProjectScripts": defaults, "projectSettingsOverrides": {"a": {"defaultProjectScripts": own}}, "projectSettingsFolded": false, "projectScriptOverrides": {"b": null, "c": legacy}});
        let ids = |scripts: Vec<Value>| scripts.iter().map(|script| script["id"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
        assert_eq!(ids(resolve(&settings, None, &[])), ["d"]);
        assert_eq!(ids(resolve(&settings, Some("a"), &[])), ["p"]);
        assert_eq!(ids(resolve(&settings, Some("b"), &[script("own", false, false)])), ["d"], "null resets to the defaults");
        assert_eq!(ids(resolve(&settings, Some("c"), &[])), ["l"]);
        assert_eq!(ids(resolve(&settings, Some("z"), &[script("own", false, false)])), ["own"], "the project's own, until the fold");
        assert_eq!(ids(resolve(&settings, Some("z"), &[])), ["d"]);
        let folded = json!({"defaultProjectScripts": defaults, "projectSettingsFolded": true, "projectScriptOverrides": {"c": legacy}});
        assert_eq!(ids(resolve(&folded, Some("c"), &[script("own", false, false)])), ["d"]);
        assert!(inherits(&folded, Some("c"), &[]) && !inherits(&settings, Some("a"), &[]) && !inherits(&settings, Some("c"), &[]) && !inherits(&settings, None, &[]));
    }

    #[test]
    fn a_save_is_one_patch_for_the_environment_or_the_whole_override_entry() {
        let settings = json!({"defaultProjectScripts": [], "projectSettingsOverrides": {"a": {"defaultRuntimeMode": "auto"}}});
        assert_eq!(save_patch(&settings, None, vec![script("x", false, false)])["patch"]["defaultProjectScripts"][0]["id"], "x");
        let project = save_patch(&settings, Some("a"), vec![script("x", false, false)]);
        let entry = &project["patch"]["projectSettingsOverrides"]["a"];
        assert_eq!((entry["defaultRuntimeMode"].as_str(), entry["defaultProjectScripts"][0]["id"].as_str()), (Some("auto"), Some("x")));
    }

    #[test]
    fn a_script_is_told_where_it_runs() {
        assert_eq!(runtime_env("/p", None), json!({"T3CODE_PROJECT_ROOT": "/p"}));
        assert_eq!(runtime_env("/p", Some("/w")), json!({"T3CODE_PROJECT_ROOT": "/p", "T3CODE_WORKTREE_PATH": "/w"}));
        assert_eq!(runtime_env("/p", Some("")), json!({"T3CODE_PROJECT_ROOT": "/p"}));
    }
}
