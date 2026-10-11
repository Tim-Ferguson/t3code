//! The Storage and Archived threads settings pages and the project address, through the mounted UI and the real native
//! server (whose settings document is what the pages write): the cleanup rules at the environment and per project, the
//! archived list with its way back, and `/projects/<key>`. Waits are on the settings the server reports.
use super::diff_tests::{Fixture, error, rendered_text, seed, until, wait_for_rendered_text};
use super::routing_tests::{Browser, BrowserDocument, last_push};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::View;
use crate::runtime::transport_tests::{change_control, click_control, control, control_attribute};
use std::{cell::RefCell, rc::Rc};

const WRITER: [AuthEnvironmentScope; 3] = [AuthEnvironmentScope::OrchestrationRead, AuthEnvironmentScope::OrchestrationOperate, AuthEnvironmentScope::SettingsWrite];

fn setting(state: Store<UiModel>, key: &str) -> Value {
    state.peek().config["settings"][key].clone()
}
fn disabled(dom: &VirtualDom, label: &str) -> bool {
    control_attribute(dom, label, "disabled").and_then(|(_, value)| value).as_deref() == Some("true")
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_storage_page_writes_the_environments_cleanup_rules_and_a_projects_own_policy() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().typed_config.is_some() && state.peek().config["settings"].is_object()).await;
    state.view().set(View::Storage);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the page, once the grant is known", |dom| control(dom, "Delete worktrees with deleted threads").is_some() && !disabled(dom, "Delete worktrees with deleted threads")).await;
    // The General page no longer holds these.
    assert!(setting(state, "storageCleanup")["worktreeOnDelete"] == json!(false));

    // Environment rules go to `storageCleanup`, one patch each.
    change_control(&mut dom, "Delete worktrees with deleted threads", "true");
    until(&mut dom, "the rule saved", |_| setting(state, "storageCleanup")["worktreeOnDelete"] == json!(true)).await;
    until(&mut dom, "the switch free", |dom| !disabled(dom, "Delete worktrees with deleted threads")).await;
    change_control(&mut dom, "Delete old rotated logs", "true");
    until(&mut dom, "the log retention saved", |_| setting(state, "storageCleanup")["logsAfterDays"] == json!(8)).await;
    until(&mut dom, "the switch free", |dom| !disabled(dom, "Delete old rotated logs")).await;

    // A project inherits until it is given a policy of its own; a custom policy starts from the rules in force.
    change_control(&mut dom, "Storage settings scope", "project");
    until(&mut dom, "the project's policy control", |dom| control(dom, "Automatic worktree cleanup").is_some()).await;
    assert!(rendered_text(&dom).contains("Use each machine's worktree cleanup settings."));
    assert!(control(&dom, "Delete old rotated logs").is_none(), "artifacts and logs are the environment's");
    change_control(&mut dom, "Automatic worktree cleanup", "off");
    until(&mut dom, "the project turned off", |_| setting(state, "projectSettingsOverrides")["project"]["worktreeCleanup"]["mode"] == json!("off")).await;
    wait_for_rendered_text(&mut dom, "Keep this project's worktrees until you delete them manually.").await;
    assert!(control(&dom, "Delete merged worktrees").is_none(), "no rules to edit while it is off");
    until(&mut dom, "the control free", |dom| !disabled(dom, "Automatic worktree cleanup")).await;
    change_control(&mut dom, "Automatic worktree cleanup", "custom");
    until(&mut dom, "the project custom", |_| setting(state, "projectSettingsOverrides")["project"]["worktreeCleanup"]["mode"] == json!("custom")).await;
    until(&mut dom, "the rules", |dom| control(dom, "Delete merged worktrees").is_some() && !disabled(dom, "Delete merged worktrees")).await;
    let rules = setting(state, "projectSettingsOverrides")["project"]["worktreeCleanup"]["rules"].clone();
    assert_eq!(rules["worktreeOnDelete"], json!(true), "the environment's rule in force is where a custom policy starts: {rules}");
    change_control(&mut dom, "Delete merged worktrees", "true");
    until(&mut dom, "the project's rule", |_| setting(state, "projectSettingsOverrides")["project"]["worktreeCleanup"]["rules"]["worktreeOnMerge"] == json!(true)).await;
    assert_eq!(setting(state, "storageCleanup")["worktreeOnMerge"], json!(false), "the environment's rule is untouched");
    until(&mut dom, "the control free", |dom| !disabled(dom, "Automatic worktree cleanup")).await;
    change_control(&mut dom, "Automatic worktree cleanup", "inherit");
    until(&mut dom, "the override gone", |_| setting(state, "projectSettingsOverrides")["project"].is_null()).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_archived_page_lists_every_projects_archived_threads_and_brings_one_back() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    dom.in_scope(ScopeId::APP, || crate::runtime::command(&props.transport, state, "thread.archive", json!({"threadId":"thread-b"}))).unwrap();
    until(&mut dom, "thread-b archived", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().all(|thread| thread.id.as_str() != "thread-b" || thread.archived_at.is_some()))).await;
    state.view().set(View::Archived);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the archived thread", |dom| control(dom, "Unarchive thread-b").is_some()).await;
    assert!(control(&dom, "Unarchive thread-a").is_none(), "only archived threads are listed");
    assert!(rendered_text(&dom).contains("Project"), "grouped under its project");
    click_control(&mut dom, "Unarchive thread-b");
    until(&mut dom, "the list empty", |dom| rendered_text(dom).contains("No archived threads yet.")).await;
    assert!(state.peek().shell.snapshot.as_ref().unwrap().threads.iter().all(|thread| thread.archived_at.is_none()));
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_project_address_opens_that_projects_settings_and_the_new_pages_have_addresses() {
    let fixture = Fixture::start(false).await;
    let browser = Rc::new(RefCell::new(Browser::default()));
    let (mut dom, props) = fixture.client_with(Gate::default(), &WRITER, {
        let browser = browser.clone();
        move |dom| {
            dom.provide_root_context(crate::routing::Enabled);
            dom.provide_root_context(Rc::new(BrowserDocument { owner: Default::default(), browser }) as Rc<dyn document::Document>);
        }
    });
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let environment = state.peek().destination.as_ref().unwrap().to_string();
    for (address, view) in [("/settings/storage", View::Storage), ("/settings/archived", View::Archived)] {
        browser.borrow_mut().go(address);
        until(&mut dom, address, |_| *state.view().peek() == view).await;
        // The address is written by an effect that leaves nothing on the page to wait for.
        super::diff_tests::pump(&mut dom).await;
        until(&mut dom, "the address kept", |_| last_push(&browser).as_deref() == Some(address)).await;
    }
    // `/projects/<key>` is a redirect to `/settings/projects`, for a bare project id or an environment-scoped key.
    for key in ["project".to_owned(), format!("{environment}:project")] {
        state.view().set(View::Chat);
        browser.borrow_mut().go(&format!("/projects/{}", key.replace(':', "%3A")));
        until(&mut dom, "the projects page", |_| *state.view().peek() == View::Projects).await;
        assert_eq!(state.peek().settings_project.as_deref(), Some("project"));
        until(&mut dom, "the address rewritten", |_| browser.borrow().sent.iter().rev().take(3).any(|message| message["type"] == "replace" && message["path"] == "/settings/projects")).await;
    }
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn the_environment_icon_is_chosen_from_the_connections_page_and_the_detected_kind_clears_the_choice() {
    let fixture = Fixture::start_offering(json!({"environmentIcon": true})).await;
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["settings"].is_object()).await;
    state.view().set(View::Connections);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    until(&mut dom, "the control", |dom| control(dom, "Environment icon").is_some_and(|_| !disabled(dom, "Environment icon"))).await;
    // The fixture detects no machine kind, so a server is the default; any other kind is a choice, kept in the settings.
    assert!(rendered_text(&dom).contains("Server (default)"));
    change_control(&mut dom, "Environment icon", "laptop");
    until(&mut dom, "the choice saved", |_| setting(state, "environmentIcon") == json!("laptop")).await;
    change_control(&mut dom, "Environment icon", "server");
    until(&mut dom, "the choice cleared", |_| setting(state, "environmentIcon").is_null()).await;
    assert_eq!(error(state), None);
    fixture.finish(dom).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn a_server_too_old_to_keep_an_icon_says_so_and_the_picker_stays_locked() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.client(Gate::default(), &WRITER);
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    until(&mut dom, "the configuration", |_| state.peek().config["settings"].is_object()).await;
    state.view().set(View::Connections);
    dom.render_immediate(&mut dioxus::dioxus_core::NoOpMutations);
    wait_for_rendered_text(&mut dom, "This environment's server is too old to keep an icon. Update it to choose one.").await;
    assert!(disabled(&dom, "Environment icon"));
    fixture.finish(dom).await;
}
