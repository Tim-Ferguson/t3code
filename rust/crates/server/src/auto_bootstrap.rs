//! `--auto-bootstrap-project-from-cwd` (`resolveAutoBootstrapWelcomeTargets` in `serverRuntimeStartup.ts`): the server registers a
//! project for its working directory when none is, and a first thread in it when the project has none, then names both in the
//! `welcome` lifecycle event so a client opens straight into them. The project's own default model and runtime mode win over the
//! environment's; the thread otherwise gets the bootstrap default (`codex`, `DEFAULT_MODEL`).
use crate::{launch::ThreadLaunchService, persistence::Store, project::ProjectService, thread::ThreadService};
use chrono::Utc;
use serde_json::{Value, json};
use std::path::Path;

/// `getAutoBootstrapThreadModelSelection`.
pub fn thread_model_selection() -> Value {
    json!({"instanceId":"codex","model":"gpt-6-astra"})
}

/// The ids to put in the `welcome` event: the project of `cwd` and its first thread.
pub fn run(store: &Store, settings: &Value, cwd: &Path) -> Result<(String, String), String> {
    let root = cwd.to_string_lossy().into_owned();
    let projects = ProjectService::new(store.clone());
    let existing = projects.list().map_err(|error| error.to_string())?.into_iter().find(|project| project["workspaceRoot"] == root.as_str());
    let project = match existing {
        Some(project) => project,
        None => {
            let title = cwd.file_name().map(|name| name.to_string_lossy().into_owned()).filter(|name| !name.is_empty()).unwrap_or_else(|| "project".into());
            projects
                .mutate(json!({"type":"project.create","commandId":uuid::Uuid::new_v4().to_string(),"projectId":uuid::Uuid::new_v4().to_string(),"title":title,"workspaceRoot":root}), Utc::now())
                .map_err(|error| error.to_string())?
        }
    };
    let project_id = project["id"].as_str().ok_or("The bootstrapped project has no id.")?.to_owned();
    let shell = ThreadService::new(store.clone()).shell_snapshot().map_err(|error| error.to_string())?;
    let existing_thread = shell["threads"].as_array().into_iter().flatten().find(|thread| thread["projectId"] == project_id.as_str() && thread["lineage"]["relationshipToParent"] != "subagent");
    if let Some(thread) = existing_thread {
        return Ok((project_id, thread["id"].as_str().unwrap_or_default().to_owned()));
    }
    // A project's own choice wins over the environment's.
    let overrides = &settings["projectSettingsOverrides"][project_id.as_str()];
    let pick = |key: &str| [&overrides[key], &settings[key]].into_iter().find(|value| !value.is_null()).cloned();
    let launched = ThreadLaunchService::new(store.clone())
        .launch(
            json!({
                "commandId": uuid::Uuid::new_v4().to_string(),
                "projectId": project_id,
                "title": "New thread",
                "modelSelection": pick("defaultModelSelection").unwrap_or_else(thread_model_selection),
                "interactionMode": "default",
                "runtimeMode": pick("defaultRuntimeMode").unwrap_or_else(|| json!("full-access")),
                "workspaceStrategy": {"type": "root"},
                "createdBy": "system",
                "creationSource": "server",
            }),
            Utc::now(),
        )
        .map_err(|error| error.to_string())?;
    let thread_id = launched["threadId"].as_str().ok_or("The bootstrap launch named no thread.")?.to_owned();
    Ok((project_id, thread_id))
}
