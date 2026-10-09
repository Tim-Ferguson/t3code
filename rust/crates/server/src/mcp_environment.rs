//! The environment and project tools. Changing a project or the environment's preferences needs a
//! full-access/default caller (`writesEnvironment`); each handler calls the same service as the
//! socket: `ProjectService`, `ManagedProjectFolders`, the repository clone and the settings service.
use crate::{
    mcp_access::{self, Access},
    mcp_control::McpControlTools,
    mcp_invocation::{InvocationScope, McpFailure, refusal},
    mcp_threads::command_id,
    project::{ProjectCommand, ProjectFailure, ProjectService},
    project_folders::ManagedProjectFolders,
    source_control_registry::SourceControlRegistry,
    source_control_repository::SourceControlRepositoryService,
};
use serde_json::{Value, json};
use t3_contracts::{ServerSettings, ServerSettingsPatch, SourceControlCloneRepositoryInput};

type Out = Result<Value, McpFailure>;

const INSTRUCTION_LIMIT: usize = 4000;

/// `preferences`: the settings the tools expose, the writing instructions cut to their limit.
pub(crate) fn preferences(settings: &ServerSettings) -> Value {
    let all = serde_json::to_value(settings).unwrap_or(Value::Null);
    let style = &all["sourceControlWritingStyle"];
    let instructions = style["customInstructions"].as_str().unwrap_or_default();
    let mut shown = instructions.chars();
    let head: String = shown.by_ref().take(INSTRUCTION_LIMIT).collect();
    let mut result = json!({
        "newWorktreesStartFromOrigin":all["newWorktreesStartFromOrigin"],
        "enableProviderUpdateChecks":all["enableProviderUpdateChecks"],
        "backgroundActivity":{"profile":all["backgroundActivity"]["profile"]},
        "sourceControlWritingStyle":{"mode":style["mode"],"followChangeRequestTemplates":style["followChangeRequestTemplates"],"customInstructions":head,"truncated":shown.next().is_some()},
    });
    if let Some(mode) = all.get("defaultThreadEnvMode").filter(|mode| !mode.is_null()) {
        result["defaultThreadEnvMode"] = mode.clone();
    }
    result
}

/// `ProjectServiceError` as the project tools report it.
fn project_failure(error: ProjectFailure) -> McpFailure {
    match error {
        ProjectFailure::Operation => mcp_access::unavailable(),
        ProjectFailure::NotFound => refusal("invalid_request", "The project was not found."),
        ProjectFailure::Conflict => refusal("invalid_request", "The workspace is already registered to a project."),
        ProjectFailure::NotEmpty => refusal("invalid_request", "The project is not empty; force=true is required to delete it."),
    }
}

impl McpControlTools {
    fn gate_environment(&self, scope: &InvocationScope) -> Result<(), McpFailure> {
        mcp_access::check_store(&self.store, scope, Access::WritesEnvironment, &[]).map(|_| ())
    }

    /// The caller, the environment descriptor and its settings: a credential of another environment reads nothing.
    async fn environment_access(&self, scope: &InvocationScope) -> Result<(Value, ServerSettings), McpFailure> {
        self.reading_caller(scope)?;
        let descriptor = self.services.environment.clone().ok_or_else(mcp_access::unavailable)?;
        if descriptor["environmentId"] != scope.environment_id.as_str() {
            return Err(refusal("capability_denied", "This credential belongs to another environment."));
        }
        let settings = self.services.settings.as_ref().ok_or_else(mcp_access::unavailable)?;
        Ok((descriptor, settings.snapshot().await.map_err(|_| mcp_access::unavailable())?))
    }

    pub(crate) async fn environment_read(&self, scope: &InvocationScope) -> Out {
        let (descriptor, settings) = self.environment_access(scope).await?;
        Ok(json!({"environmentId":descriptor["environmentId"],"label":descriptor["label"],"serverVersion":descriptor["serverVersion"],"platform":descriptor["platform"],"preferences":preferences(&settings)}))
    }

    pub(crate) async fn environment_preferences_update(&self, scope: &InvocationScope, args: Value) -> Out {
        self.gate_environment(scope)?;
        let patch: ServerSettingsPatch = serde_json::from_value(args).map_err(|_| mcp_access::unavailable())?;
        // The caller's modes can change while a write waits for the settings service: ask again just before it.
        let (_, _) = self.environment_access(scope).await?;
        self.gate_environment(scope)?;
        let settings = self.services.settings.as_ref().ok_or_else(mcp_access::unavailable)?;
        Ok(preferences(&settings.update(patch).await.map_err(|_| mcp_access::unavailable())?))
    }

    // ---- projects -----------------------------------------------------------------------------

    fn projects(&self) -> ProjectService {
        ProjectService::new(self.store.clone())
    }
    fn live(&self, id: &str) -> Out {
        let mut project = self.projects().get(id).map_err(|_| mcp_access::unavailable())?.ok_or_else(mcp_access::unavailable)?;
        project["deletedAt"] = Value::Null;
        Ok(project)
    }
    fn project_command(kind: &str, id: &str, project_id: &str, fields: Value) -> Result<ProjectCommand, McpFailure> {
        let mut command = json!({"type":kind,"commandId":id,"projectId":project_id});
        if let (Some(target), Some(fields)) = (command.as_object_mut(), fields.as_object()) {
            target.extend(fields.clone());
        }
        ProjectCommand::from_json(command).map_err(|_| mcp_access::unavailable())
    }

    pub(crate) async fn project_create(&self, scope: &InvocationScope, args: Value) -> Out {
        self.gate_environment(scope)?;
        let Some(root) = args["workspaceRoot"].as_str() else {
            // Creation records no model default (only an update does), so refuse what this form would drop.
            if ["scripts", "createWorkspaceRootIfMissing", "defaultModelSelection"].iter().any(|key| args.get(*key).is_some()) {
                return Err(refusal("invalid_request", "A project started from its title takes only a title; set scripts or defaultModelSelection afterwards with t3_project_update."));
            }
            let folders = ManagedProjectFolders::for_store(&self.store).ok_or_else(mcp_access::unavailable)?;
            let created = folders.create_named_project(args["title"].as_str().unwrap_or_default()).await.map_err(|error| refusal("orchestration_error", error.message()))?;
            let mut project = self.live(&created.project_id)?;
            if let Some(error) = created.commit_error.filter(|error| !error.is_empty()) {
                project["commitError"] = json!(error);
            }
            return Ok(project);
        };
        let (root, create) = (root.to_owned(), args["createWorkspaceRootIfMissing"] == true);
        let normalized = tokio::task::spawn_blocking(move || crate::project::normalize_workspace_root(&root, create)).await.map_err(|_| mcp_access::unavailable())?.map_err(|_| mcp_access::unavailable())?;
        let id = command_id();
        let mut fields = json!({"title":args["title"],"workspaceRoot":normalized});
        if let Some(scripts) = args.get("scripts") {
            fields["scripts"] = scripts.clone();
        }
        let command = Self::project_command("project.create", &id, &id, fields)?;
        self.projects().apply(&command, (self.clock)()).map_err(project_failure)
    }

    pub(crate) async fn project_update(&self, scope: &InvocationScope, args: Value) -> Out {
        self.gate_environment(scope)?;
        let project_id = args["projectId"].as_str().unwrap_or_default().to_owned();
        let existing = self.projects().get(&project_id).map_err(|_| mcp_access::unavailable())?.ok_or_else(|| project_failure(ProjectFailure::NotFound))?;
        let mut fields = serde_json::Map::new();
        for key in ["title", "defaultModelSelection", "autoPull", "projectIcon", "faviconPath", "defaultThreadEnvMode", "scripts"] {
            if let Some(value) = args.get(key) {
                fields.insert(key.into(), value.clone());
            }
        }
        if let Some(root) = args["workspaceRoot"].as_str() {
            let root = root.to_owned();
            let normalized = tokio::task::spawn_blocking(move || crate::project::normalize_workspace_root(&root, false)).await.map_err(|_| mcp_access::unavailable())?.map_err(|_| mcp_access::unavailable())?;
            if existing["workspaceRoot"] != normalized.as_str() {
                fields.insert("workspaceRoot".into(), json!(normalized));
            }
        }
        let command = Self::project_command("project.meta.update", &command_id(), &project_id, Value::Object(fields))?;
        self.projects().apply(&command, (self.clock)()).map_err(project_failure)
    }

    pub(crate) async fn project_delete(&self, scope: &InvocationScope, args: Value) -> Out {
        self.gate_environment(scope)?;
        let project_id = args["projectId"].as_str().unwrap_or_default();
        if self.store.projection("project", project_id).map_err(|_| mcp_access::unavailable())?.is_none() {
            return Err(project_failure(ProjectFailure::NotFound));
        }
        let fields = if let Some(force) = args.get("force") { json!({"force":force}) } else { json!({}) };
        let command = Self::project_command("project.delete", &command_id(), project_id, fields)?;
        self.projects().apply(&command, (self.clock)()).map_err(project_failure)
    }

    pub(crate) async fn project_clone(&self, scope: &InvocationScope, args: Value) -> Out {
        self.gate_environment(scope)?;
        let input: SourceControlCloneRepositoryInput = serde_json::from_value(args).map_err(|_| mcp_access::unavailable())?;
        let host = crate::source_control_host::Host::process_default().with_settings(self.services.settings.clone());
        let service = SourceControlRepositoryService::new(SourceControlRegistry::new(host));
        let cloned = Box::pin(service.clone_repository(&input)).await.map_err(|error| refusal("orchestration_error", &error.detail))?;
        serde_json::to_value(cloned).map_err(|_| mcp_access::unavailable())
    }
}
