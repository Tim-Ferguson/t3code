//! First-run onboarding: when the welcome wizard is shown, what its agent cards say, and the commands its setup
//! terminal pre-types. Ports of `onboarding/firstRun.logic.ts`, `onboarding/providerReadiness.logic.ts` and the
//! landing rule of `onboarding/projectImport.logic.ts`.
use serde_json::Value;
use std::collections::BTreeMap;
use t3_contracts::{ServerProvider, ServerProviderAuthStatus, ServerProviderState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decision {
    #[default]
    Pending,
    App,
    Wizard,
}

/// What the gate has settled on, and whether it waited so long that the person should be offered a reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gate {
    pub decision: Decision,
    pub stalled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateEvent {
    Evidence(Decision),
    Timeout,
}

/// `transitionFirstRunGateState`: a wizard stays a wizard, a pending answer changes nothing, and an app only gives way to a wizard.
pub fn transition(state: Gate, event: GateEvent) -> Gate {
    match event {
        GateEvent::Timeout => {
            if state.decision == Decision::Pending && !state.stalled {
                Gate { stalled: true, ..state }
            } else {
                state
            }
        }
        GateEvent::Evidence(decision) => {
            if state.decision == Decision::Wizard || decision == Decision::Pending || (state.decision == Decision::App && decision != Decision::Wizard) {
                state
            } else {
                Gate { decision, stalled: false }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProjectFact<'a> {
    pub id: &'a str,
    pub environment_id: &'a str,
    pub workspace_root: &'a str,
}
#[derive(Debug, Clone, Copy)]
pub struct ThreadFact<'a> {
    pub id: &'a str,
    pub project_id: &'a str,
    pub environment_id: &'a str,
    pub has_run: bool,
    pub has_user_message: bool,
    pub has_runtime: bool,
}
/// What the server's welcome said about the project and thread it made from its working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Welcome {
    pub bootstrap_project_id: Option<String>,
    pub bootstrap_thread_id: Option<String>,
    pub bootstrap_project_created: Option<bool>,
    pub bootstrap_thread_created: Option<bool>,
    /// `pending` or `complete`; older servers send none.
    pub bootstrap_status: Option<String>,
}
impl Welcome {
    pub fn from_event(event: &Value) -> Option<Self> {
        (event["type"] == "welcome").then(|| {
            let payload = &event["payload"];
            let text = |key: &str| payload[key].as_str().map(str::to_owned);
            Self {
                bootstrap_project_id: text("bootstrapProjectId"),
                bootstrap_thread_id: text("bootstrapThreadId"),
                bootstrap_project_created: payload["bootstrapProjectCreated"].as_bool(),
                bootstrap_thread_created: payload["bootstrapThreadCreated"].as_bool(),
                bootstrap_status: text("bootstrapStatus"),
            }
        })
    }
}

/// `isFirstRunWorkspaceProvenanceAuthoritative`: an empty catalog is not final while the cwd bootstrap is still pending.
pub fn provenance_authoritative(welcome: Option<&Welcome>) -> bool {
    welcome.is_some_and(|welcome| welcome.bootstrap_status.as_deref() != Some("pending"))
}

/// `isFreshFirstRunWorkspace`: only the project and thread this startup made count as a fresh workspace.
pub fn is_fresh_workspace(primary_environment_id: Option<&str>, server_cwd: Option<&str>, welcome: Option<&Welcome>, projects: &[ProjectFact<'_>], threads: &[ThreadFact<'_>]) -> bool {
    if projects.len() > 1 || threads.len() > 1 {
        return false;
    }
    let welcome = welcome.cloned().unwrap_or_default();
    if let Some(project) = projects.first() {
        let same_root = server_cwd.is_some_and(|cwd| crate::project_import::comparable(project.workspace_root) == crate::project_import::comparable(cwd));
        if welcome.bootstrap_project_created != Some(true) || welcome.bootstrap_project_id.as_deref() != Some(project.id) || !same_root || Some(project.environment_id) != primary_environment_id {
            return false;
        }
    }
    let Some(thread) = threads.first() else { return true };
    projects.first().is_some_and(|project| {
        welcome.bootstrap_thread_created == Some(true)
            && welcome.bootstrap_thread_id.as_deref() == Some(thread.id)
            && Some(thread.environment_id) == primary_environment_id
            && thread.project_id == project.id
            && !thread.has_run
            && !thread.has_user_message
            && !thread.has_runtime
    })
}

#[derive(Debug, Clone, Copy)]
pub struct DecisionInput {
    pub enabled: bool,
    pub hydrated: bool,
    pub completed: bool,
    pub bootstrapped: bool,
    pub authoritative: bool,
    pub workspace_authoritative: bool,
    pub workspace_provenance_authoritative: bool,
    pub catalog_ready: bool,
    pub server_config_available: bool,
    pub workspace_fresh: bool,
    pub project_count: usize,
    pub thread_count: usize,
}

/// `resolveFirstRunDecision`: cached projects may open the app, but only live workspace data may complete onboarding.
/// The second value is whether to record the onboarding as completed.
pub fn resolve_decision(input: DecisionInput) -> (Decision, bool) {
    if !input.enabled || (input.hydrated && input.completed) {
        return (Decision::App, false);
    }
    if !input.hydrated {
        return (Decision::Pending, false);
    }
    if input.project_count > 1 || input.thread_count > 1 {
        return (Decision::App, input.bootstrapped && input.authoritative && input.workspace_authoritative && input.catalog_ready && input.server_config_available);
    }
    if !input.bootstrapped || !input.authoritative || !input.workspace_provenance_authoritative || !input.catalog_ready || !input.server_config_available {
        return (Decision::Pending, false);
    }
    if input.workspace_fresh { (Decision::Wizard, false) } else { (Decision::App, input.workspace_authoritative) }
}

// ---- agents -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderState {
    Checking,
    Disabled,
    Install,
    Attention,
    SignIn,
    Ready,
}

/// `getOnboardingProviderState`; `None` is a provider the server has not reported yet.
pub fn provider_state(provider: Option<&ServerProvider>) -> ProviderState {
    let Some(provider) = provider else { return ProviderState::Checking };
    if !provider.enabled || provider.status == ServerProviderState::Disabled {
        return ProviderState::Disabled;
    }
    if !provider.installed && provider.status == ServerProviderState::Warning && provider.auth.status == ServerProviderAuthStatus::Unknown {
        return ProviderState::Checking;
    }
    if !provider.installed {
        return ProviderState::Install;
    }
    if provider.auth.status == ServerProviderAuthStatus::Unauthenticated {
        return ProviderState::SignIn;
    }
    if provider.status == ServerProviderState::Ready { ProviderState::Ready } else { ProviderState::Attention }
}

/// `selectOnboardingProvidersByDriver`: the most usable configured instance of each driver (the first of equals).
pub fn by_driver(providers: &[ServerProvider]) -> BTreeMap<String, &ServerProvider> {
    let mut chosen: BTreeMap<String, &ServerProvider> = BTreeMap::new();
    for provider in providers {
        let driver = provider.driver.to_string();
        match chosen.get(&driver) {
            Some(existing) if provider_state(Some(provider)) <= provider_state(Some(existing)) => {}
            _ => {
                chosen.insert(driver, provider);
            }
        }
    }
    chosen
}

/// Official standalone installers: neither needs Node or npm, and both land where the server's provider maintenance recognizes them.
pub fn install_command(driver: &str, os: &str) -> &'static str {
    match (driver, os == "windows") {
        ("claudeAgent", true) => "irm https://claude.ai/install.ps1 | iex",
        ("claudeAgent", false) => "curl -fsSL https://claude.ai/install.sh | bash",
        (_, true) => "irm https://chatgpt.com/codex/install.ps1 | iex",
        (_, false) => "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
    }
}

fn quote_binary(path: &str, fallback: &str, os: &str) -> String {
    let safe = !path.is_empty() && path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '\\' | '-'));
    if safe && (os == "windows" || !path.contains('\\')) {
        return path.to_owned();
    }
    match os {
        "windows" => format!("& '{}'", path.replace('\'', "''")),
        "darwin" | "linux" => match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            Some(rest) => format!("~/'{}'", rest.replace('\'', "'\"'\"'")),
            None => format!("'{}'", path.replace('\'', "'\"'\"'")),
        },
        _ => fallback.to_owned(),
    }
}

/// `resolveOnboardingProviderLoginCommand`: the instance's own binary, from its configuration or the driver's settings.
pub fn login_command(provider: &ServerProvider, settings: &Value, os: &str) -> String {
    let driver = provider.driver.to_string();
    let (fallback, verb) = match driver.as_str() {
        "claudeAgent" => ("claude", "auth login"),
        "codex" => ("codex", "login"),
        _ => return driver,
    };
    let config = match settings["providerInstances"].get(provider.instance_id.as_str()) {
        Some(instance) => &instance["config"],
        None => &settings["providers"][driver.as_str()],
    };
    let binary = config["binaryPath"].as_str().unwrap_or(fallback);
    format!("{} {verb}", quote_binary(binary, fallback, os))
}

/// `resolveOnboardingLandingProject`: where the person starts after an import: a selected project with imported history, then a completed empty import.
pub fn landing_project<'a, T>(selection: &[String], with_history: &'a BTreeMap<String, T>, completed: &'a BTreeMap<String, T>) -> Option<&'a T> {
    selection.iter().find_map(|path| with_history.get(path)).or_else(|| selection.iter().find_map(|path| completed.get(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(value: Value) -> ServerProvider {
        let mut base = json!({"instanceId":"codex","driver":"codex","enabled":true,"installed":true,"version":null,"status":"ready","auth":{"status":"authenticated"},"checkedAt":"2026-10-11T00:00:00.000Z","models":[],"slashCommands":[],"skills":[]});
        for (key, value) in value.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn a_provider_card_says_what_to_do_next() {
        assert_eq!(provider_state(None), ProviderState::Checking);
        assert_eq!(provider_state(Some(&provider(json!({})))), ProviderState::Ready);
        assert_eq!(provider_state(Some(&provider(json!({"enabled":false})))), ProviderState::Disabled);
        assert_eq!(provider_state(Some(&provider(json!({"installed":false,"status":"warning","auth":{"status":"unknown"}})))), ProviderState::Checking);
        assert_eq!(provider_state(Some(&provider(json!({"installed":false,"status":"error","auth":{"status":"unknown"}})))), ProviderState::Install);
        assert_eq!(provider_state(Some(&provider(json!({"auth":{"status":"unauthenticated"},"status":"error"})))), ProviderState::SignIn);
        assert_eq!(provider_state(Some(&provider(json!({"status":"warning"})))), ProviderState::Attention);
    }

    #[test]
    fn the_most_usable_instance_of_a_driver_is_the_one_shown() {
        let signed_out = provider(json!({"instanceId":"a","auth":{"status":"unauthenticated"},"status":"error"}));
        let ready = provider(json!({"instanceId":"b"}));
        let other_ready = provider(json!({"instanceId":"c"}));
        let all = [signed_out, ready, other_ready];
        assert_eq!(by_driver(&all)["codex"].instance_id.as_str(), "b", "ready outranks sign-in, and the first of equals stays");
    }

    #[test]
    fn the_setup_terminal_gets_the_instances_own_binary_quoted_for_its_shell() {
        let codex = provider(json!({}));
        let settings = json!({"providers":{"codex":{"binaryPath":"/opt/my codex/bin/codex"}},"providerInstances":{}});
        assert_eq!(login_command(&codex, &settings, "darwin"), "'/opt/my codex/bin/codex' login");
        assert_eq!(login_command(&codex, &json!({"providers":{"codex":{"binaryPath":"codex"}}}), "linux"), "codex login");
        assert_eq!(login_command(&codex, &json!({"providers":{"codex":{"binaryPath":"C:\\Tools\\codex.exe"}}}), "windows"), "C:\\Tools\\codex.exe login");
        assert_eq!(login_command(&codex, &json!({"providers":{"codex":{"binaryPath":"~/bin/it's"}}}), "linux"), "~/'bin/it'\"'\"'s' login");
        let instance = json!({"providers":{},"providerInstances":{"codex":{"config":{"binaryPath":"/usr/local/bin/codex2"}}}});
        assert_eq!(login_command(&codex, &instance, "linux"), "/usr/local/bin/codex2 login");
        let claude = provider(json!({"instanceId":"claudeAgent","driver":"claudeAgent"}));
        assert_eq!(login_command(&claude, &json!({"providers":{}}), "linux"), "claude auth login");
        assert_eq!(install_command("claudeAgent", "darwin"), "curl -fsSL https://claude.ai/install.sh | bash");
        assert_eq!(install_command("codex", "windows"), "irm https://chatgpt.com/codex/install.ps1 | iex");
    }

    fn input() -> DecisionInput {
        DecisionInput { enabled: true, hydrated: true, completed: false, bootstrapped: true, authoritative: true, workspace_authoritative: true, workspace_provenance_authoritative: true, catalog_ready: true, server_config_available: true, workspace_fresh: true, project_count: 1, thread_count: 1 }
    }

    #[test]
    fn a_fresh_install_gets_the_wizard_and_nothing_else_does() {
        assert_eq!(resolve_decision(input()), (Decision::Wizard, false));
        assert_eq!(resolve_decision(DecisionInput { completed: true, ..input() }), (Decision::App, false), "once finished it never comes back");
        assert_eq!(resolve_decision(DecisionInput { hydrated: false, ..input() }), (Decision::Pending, false), "settings are not read yet");
        assert_eq!(resolve_decision(DecisionInput { bootstrapped: false, ..input() }), (Decision::Pending, false), "the workspace is not known yet");
        assert_eq!(resolve_decision(DecisionInput { workspace_fresh: false, ..input() }), (Decision::App, true), "an existing workspace completes it");
        assert_eq!(resolve_decision(DecisionInput { project_count: 2, ..input() }), (Decision::App, true));
        assert_eq!(resolve_decision(DecisionInput { enabled: false, ..input() }), (Decision::App, false));
    }

    #[test]
    fn once_the_wizard_is_chosen_the_gate_keeps_it_and_a_stall_is_reported_once() {
        let pending = Gate::default();
        assert_eq!(transition(pending, GateEvent::Timeout), Gate { decision: Decision::Pending, stalled: true });
        let wizard = transition(pending, GateEvent::Evidence(Decision::Wizard));
        assert_eq!(wizard, Gate { decision: Decision::Wizard, stalled: false });
        assert_eq!(transition(wizard, GateEvent::Evidence(Decision::App)), wizard);
        let app = transition(pending, GateEvent::Evidence(Decision::App));
        assert_eq!(transition(app, GateEvent::Evidence(Decision::Pending)), app);
        assert_eq!(transition(app, GateEvent::Evidence(Decision::Wizard)).decision, Decision::Wizard, "late evidence of a fresh install still wins over an app");
        assert_eq!(transition(app, GateEvent::Timeout), app);
    }

    #[test]
    fn only_the_project_and_thread_this_startup_made_are_a_fresh_workspace() {
        let welcome = Welcome { bootstrap_project_id: Some("p".into()), bootstrap_thread_id: Some("t".into()), bootstrap_project_created: Some(true), bootstrap_thread_created: Some(true), bootstrap_status: Some("complete".into()) };
        let project = ProjectFact { id: "p", environment_id: "e", workspace_root: "/work/app/" };
        let thread = ThreadFact { id: "t", project_id: "p", environment_id: "e", has_run: false, has_user_message: false, has_runtime: false };
        assert!(is_fresh_workspace(Some("e"), Some("/work/app"), Some(&welcome), &[], &[]), "nothing at all");
        assert!(is_fresh_workspace(Some("e"), Some("/work/app"), Some(&welcome), &[project], &[thread]));
        assert!(!is_fresh_workspace(Some("e"), Some("/work/app"), Some(&welcome), &[project], &[ThreadFact { has_user_message: true, ..thread }]), "the person already wrote to it");
        assert!(!is_fresh_workspace(Some("e"), Some("/elsewhere"), Some(&welcome), &[project], &[thread]), "a project of another folder is the person's own");
        assert!(!is_fresh_workspace(Some("e"), Some("/work/app"), Some(&Welcome { bootstrap_project_created: Some(false), ..welcome.clone() }), &[project], &[]), "not created by this startup");
        assert!(!is_fresh_workspace(Some("e"), Some("/work/app"), Some(&welcome), &[project, ProjectFact { id: "q", ..project }], &[]));
        assert!(provenance_authoritative(Some(&welcome)));
        assert!(!provenance_authoritative(Some(&Welcome { bootstrap_status: Some("pending".into()), ..welcome })));
        assert!(!provenance_authoritative(None));
    }

    #[test]
    fn the_person_lands_in_a_project_with_history_before_one_that_only_finished() {
        let history = BTreeMap::from([("/b".to_owned(), 2)]);
        let done = BTreeMap::from([("/a".to_owned(), 1), ("/b".to_owned(), 2)]);
        assert_eq!(landing_project(&["/a".into(), "/b".into()], &history, &done), Some(&2));
        assert_eq!(landing_project(&["/a".into()], &history, &done), Some(&1));
        assert_eq!(landing_project(&["/z".into()], &history, &done), None);
    }
}
