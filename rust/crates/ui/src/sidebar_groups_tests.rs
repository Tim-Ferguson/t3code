//! Grouped project rows through the mounted UI and the real native server. Two directories are real
//! git checkouts of one repository (a remote named origin, never contacted), so the server resolves the
//! same repository identity for both and sends it on the project-only shell frame that follows the
//! snapshot. Waits are on that identity, on state, or on text that only the awaited step produces.
use super::diff_tests::{Fixture, rendered_text, seed, until};
use super::*;
use crate::diff_panel::Gate;
use crate::runtime::transport_tests::{change_control, click_control, control};

/// A directory that is a checkout of `github.com/acme/widget`.
fn checkout(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::Builder::new().prefix(&format!("t3code-rust-port-test-grouping-{name}-")).tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    for arguments in [vec!["init", "-q"], vec!["remote", "add", "origin", "https://github.com/acme/widget.git"]] {
        let status = std::process::Command::new("git").args(&arguments).current_dir(&root).env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null").status().unwrap();
        assert!(status.success());
    }
    (directory, root)
}
async fn project(dom: &mut VirtualDom, props: &super::diff_tests::Harness, state: Store<UiModel>, id: &str, title: &str, root: &std::path::Path) {
    dom.in_scope(ScopeId::APP, || request(&props.transport, state, "projects.mutate", json!({"type":"project.create","commandId":id,"projectId":id,"title":title,"workspaceRoot":root}), RequestKind::Unary)).unwrap();
    until(dom, id, |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.id.as_str() == id))).await;
}
fn has_identity(state: Store<UiModel>, id: &str) -> bool {
    state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.projects.iter().any(|project| project.id.as_str() == id && project.extra.get("repositoryIdentity").is_some_and(|identity| identity.is_object())))
}
fn rows(state: Store<UiModel>) -> Vec<t3_client::project_grouping::Group> {
    let model = state.peek();
    crate::sidebar_groups::groups_of(&model.shell.snapshot.as_ref().unwrap().projects, &model.client_settings).1
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn checkouts_of_one_repository_share_a_row_that_can_be_filtered_ungrouped_and_regrouped() {
    let fixture = Fixture::start(false).await;
    let (mut dom, props) = fixture.operator(Gate::default());
    let state = seed(&mut dom, &props, &fixture.workspace).await;
    let (_main_directory, main_root) = checkout("main");
    let (_fix_directory, fix_root) = checkout("fix");
    project(&mut dom, &props, state, "widget-main", "Widget", &main_root).await;
    project(&mut dom, &props, state, "widget-fix", "Widget fix", &fix_root).await;
    dom.in_scope(ScopeId::APP, || command(&props.transport, state, "thread.create", json!({"createdBy":"user","creationSource":"web","threadId":"thread-fix","projectId":"widget-fix","title":"thread-fix","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"full-access","interactionMode":"default","branch":null,"worktreePath":null}))).unwrap();
    until(&mut dom, "the thread", |_| state.peek().shell.snapshot.as_ref().is_some_and(|snapshot| snapshot.threads.iter().any(|thread| thread.id.as_str() == "thread-fix"))).await;
    // The identity arrives after the snapshot; until then the checkouts are rows of their own.
    until(&mut dom, "both identities", |_| has_identity(state, "widget-main") && has_identity(state, "widget-fix")).await;
    let groups = rows(state);
    assert_eq!(groups.len(), 2, "the fixture's own project and the one row of the two checkouts");
    let widget = groups.iter().find(|group| group.contains("", "widget-fix")).unwrap();
    assert_eq!(widget.project_ids(), vec!["widget-main", "widget-fix"]);
    let label = widget.label.clone();
    until(&mut dom, "the grouped row", |dom| control(dom, "2 projects grouped").is_some()).await;
    assert!(control(&dom, &format!("Project actions for {label}")).is_some());
    assert!(rendered_text(&dom).contains("thread-fix") && rendered_text(&dom).contains("thread-a"), "every thread is listed");

    // Filtering to the row hides the other project's threads, and "All projects" brings them back.
    click_control(&mut dom, "Filter threads by project");
    click_control(&mut dom, &label);
    until(&mut dom, "the filter", |dom| !rendered_text(dom).contains("thread-a") && rendered_text(dom).contains("thread-fix")).await;
    assert!(control(&dom, &format!("Filter threads by project: {label}")).is_some());
    click_control(&mut dom, &format!("Filter threads by project: {label}"));
    click_control(&mut dom, "All projects");
    until(&mut dom, "all projects", |dom| rendered_text(dom).contains("thread-a")).await;

    // One directory can opt out of the global rule: "Group into…" saves an override for it.
    click_control(&mut dom, &format!("Project actions for {label}"));
    click_control(&mut dom, &format!("Group project Widget fix ({})", fix_root.display()));
    change_control(&mut dom, "Project grouping rule", "separate");
    click_control(&mut dom, "Save grouping");
    let key = format!(":{}", fix_root.display());
    until(&mut dom, "the override", |_| state.peek().client_settings.sidebar_project_grouping_overrides.iter().any(|(existing, mode)| existing.as_str() == key && *mode == t3_contracts::SidebarProjectGroupingMode::Separate)).await;
    until(&mut dom, "two rows apart", |dom| control(dom, "2 projects grouped").is_none() && rows(state).len() == 3).await;

    // Taking the override away (inherit) groups it again; the global switch then ungroups everything.
    click_control(&mut dom, "Project actions for Widget fix");
    click_control(&mut dom, "Group project Widget fix");
    change_control(&mut dom, "Project grouping rule", "inherit");
    click_control(&mut dom, "Save grouping");
    until(&mut dom, "regrouped", |dom| control(dom, "2 projects grouped").is_some()).await;
    state.view().set(View::General);
    until(&mut dom, "the switch", |dom| control(dom, "Project grouping").is_some()).await;
    change_control(&mut dom, "Project grouping", "false");
    until(&mut dom, "separate", |_| state.peek().client_settings.sidebar_project_grouping_mode == t3_contracts::SidebarProjectGroupingMode::Separate).await;
    assert_eq!(rows(state).len(), 3);
    change_control(&mut dom, "Project grouping", "true");
    until(&mut dom, "grouped again", |_| state.peek().client_settings.sidebar_project_grouping_mode == t3_contracts::SidebarProjectGroupingMode::Repository).await;
    assert_eq!(rows(state).len(), 2);
    fixture.finish(dom).await;
}
