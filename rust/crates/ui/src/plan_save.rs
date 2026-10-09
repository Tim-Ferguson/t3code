//! "Save to workspace" on a proposed plan: the plan as markdown, written into the thread's
//! workspace at a path the reader picks. Port of the save dialog of `ProposedPlanCard.tsx`.
use crate::{
    notice::{self, Level},
    runtime::{self, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::json;

/// Where a plan is saved from: the open thread's worktree, else its project's folder.
pub(crate) fn workspace_root(state: Store<UiModel>, thread_id: &str) -> Option<String> {
    let field = state.shell();
    let shell = field.read();
    let snapshot = shell.snapshot.as_ref()?;
    let thread = snapshot.threads.iter().find(|thread| thread.id.as_str() == thread_id)?;
    thread.worktree_path.as_ref().map(ToString::to_string).or_else(|| snapshot.projects.iter().find(|project| project.id == thread.project_id).map(|project| project.workspace_root.to_string()))
}
/// Whether this connection may write files in the environment the thread lives in.
pub(crate) fn may_write(state: Store<UiModel>) -> bool {
    state.destination().read().as_ref().is_some_and(|destination| state.environments().read().allows(destination, t3_contracts::AuthEnvironmentScope::FilesystemWrite))
}

#[component]
pub(crate) fn SaveDialog(state: Store<UiModel>, transport: runtime::TransportHandle, root: String, filename: String, contents: String, close: EventHandler<()>) -> Element {
    let mut path = use_signal(|| filename.clone());
    let mut saving = use_signal(|| false);
    let mut failure = use_signal(|| None::<String>);
    let root_for_save = root.clone();
    let save = move || {
        let relative = path.peek().trim().to_owned();
        if relative.is_empty() {
            failure.set(Some("Enter a workspace path.".into()));
            return;
        }
        if *saving.peek() {
            return;
        }
        saving.set(true);
        failure.set(None);
        let (transport, root, contents) = (transport.clone(), root_for_save.clone(), contents.clone());
        spawn(async move {
            let owner = runtime::response_owner(&transport, state);
            let result = runtime::request_value_reported(transport.clone(), state, "projects.writeFile", json!({"cwd":root,"relativePath":relative,"contents":contents})).await;
            saving.set(false);
            if owner.is_none() || owner != runtime::response_owner(&transport, state) {
                return;
            }
            match result {
                Ok(value) => {
                    notice::push(state, Level::Success, "Plan saved to workspace", value["relativePath"].as_str().unwrap_or(&relative).to_owned());
                    close.call(());
                }
                Err(message) => failure.set(Some(message)),
            }
        });
    };
    let mut submit = save.clone();
    rsx! {
        div {class:"modal-backdrop",onclick:move |_| close.call(()),
            div {class:"modal",role:"dialog","aria-modal":"true","aria-label":"Save plan to workspace",onclick:move |event| event.stop_propagation(),
                onkeydown:move |event| {if event.key()==Key::Escape {event.stop_propagation();close.call(());}},
                h2 {"Save plan to workspace"}
                p {class:"muted","Saved into {root}."}
                label {r#for:"plan-save-path","Path in the workspace"}
                input {id:"plan-save-path","aria-label":"Workspace path",value:"{path}",disabled:saving(),autofocus:true,
                    oninput:move |event| path.set(event.value()),
                    onkeydown:move |event| {if event.key()==Key::Enter {event.prevent_default();submit();}}}
                if let Some(message)=failure.read().clone() {p {class:"error-banner",role:"alert","{message}"}}
                div {class:"modal-actions",
                    button {r#type:"button",onclick:move |_| close.call(()),"Cancel"}
                    button {class:"primary",r#type:"button","aria-label":"Save plan",disabled:saving(),onclick:move |_| {let mut save=save.clone();save();},if saving() {"Saving\u{2026}"} else {"Save"}}
                }
            }
        }
    }
}
