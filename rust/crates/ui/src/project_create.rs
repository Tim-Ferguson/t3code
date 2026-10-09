//! Starting work without picking a folder: a project made from just a name, and a thread with no
//! project at all (the server's "No project" workspace). Both are server calls whose failures
//! belong to whoever asked, so they come back as text instead of filling the app-wide banner.
use crate::{
    notice::{self, Level},
    runtime::{self, TransportHandle, UiModel, UiModelStoreExt},
};
use dioxus::prelude::*;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Created {
    pub project_id: String,
    pub workspace_root: String,
    pub commit_error: Option<String>,
}

/// Where the connected environment puts projects started from a name, if it said.
pub(crate) fn new_projects_root(state: Store<UiModel>) -> Option<String> {
    let field = state.typed_config();
    let config = field.read();
    config.as_ref()?.new_projects_root.as_ref().map(|root| root.as_str().to_owned())
}

/// Whether the connected environment offers threads without a project. As in the original, the
/// "No project" entry points exist only when the server's config names a scratch folder.
pub(crate) fn scratch_available(state: Store<UiModel>) -> bool {
    let field = state.typed_config();
    let config = field.read();
    config.as_ref().is_some_and(|config| config.scratch_workspace_root.is_some())
}

/// `projects.createNew`. An Err is the server's own message.
pub(crate) async fn create_named(transport: TransportHandle, state: Store<UiModel>, name: &str) -> Result<Created, String> {
    let value = runtime::request_value_reported(transport, state, "projects.createNew", json!({"name":name})).await?;
    decode_created(&value)
}
fn decode_created(value: &Value) -> Result<Created, String> {
    Ok(Created {
        project_id: value["projectId"].as_str().ok_or("The server's reply named no project.")?.to_owned(),
        workspace_root: value["workspaceRoot"].as_str().ok_or("The server's reply named no folder.")?.to_owned(),
        commit_error: value["commitError"].as_str().map(str::to_owned),
    })
}

/// Says where the project went, and warns when its first commit failed, then opens a new thread
/// in it.
pub(crate) fn finish_created(transport: &TransportHandle, state: Store<UiModel>, name: &str, created: &Created) {
    match &created.commit_error {
        None => notice::push(state, Level::Success, format!("Created {name}"), created.workspace_root.clone()),
        Some(error) => notice::push(state, Level::Warning, format!("Created {name} without a first commit"), format!("{error} The project is in {}.", created.workspace_root)),
    }
    runtime::new_thread(transport, state, Some(created.project_id.clone()));
}

/// A new thread in the environment's "No project" workspace, created on first use.
pub(crate) fn start_scratch(transport: &TransportHandle, state: Store<UiModel>) {
    let transport = transport.clone();
    // The answer may arrive after the user has gone elsewhere; it is only acted on if the
    // connection that asked still owns the app.
    let owner = runtime::response_owner(&transport, state);
    dioxus::dioxus_core::spawn_forever(async move {
        let result = runtime::request_value_reported(transport.clone(), state, "projects.ensureScratch", json!({})).await;
        if owner.is_none() || owner != runtime::response_owner(&transport, state) {
            return;
        }
        match result.and_then(|value| value["projectId"].as_str().map(str::to_owned).ok_or_else(|| "The server's reply named no project.".to_owned())) {
            Ok(project_id) => runtime::new_thread(&transport, state, Some(project_id)),
            Err(reason) => notice::push(state, Level::Error, "Could not start without a project", reason),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_without_a_commit_error_is_clean_and_one_with_it_keeps_the_text() {
        let clean = decode_created(&json!({"projectId":"p","workspaceRoot":"/h/projects/x"})).unwrap();
        assert_eq!((clean.project_id.as_str(), clean.commit_error), ("p", None));
        let warned = decode_created(&json!({"projectId":"p","workspaceRoot":"/h/x","commitError":"Git needs your name."})).unwrap();
        assert_eq!(warned.commit_error.as_deref(), Some("Git needs your name."));
        assert!(decode_created(&json!({"workspaceRoot":"/x"})).is_err());
    }
}
