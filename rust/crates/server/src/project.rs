use crate::persistence::{
    Decision, Event, Receipt, Store, StoreError, StoredEvent, read_projection, read_projections,
    write_projection,
};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::Transaction;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectCommand {
    #[serde(rename = "type")]
    pub command_type: String,
    pub command_id: String,
    pub project_id: String,
    #[serde(flatten)]
    pub fields: BTreeMap<String, Value>,
}

impl ProjectCommand {
    pub fn from_json(input: Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(input)
    }
    fn normalized(&self) -> Result<Self, StoreError> {
        let mut command = self.clone();
        command.command_id = t3_contracts::CommandId::new(&command.command_id)
            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?
            .into_string();
        command.project_id = t3_contracts::ProjectId::new(&command.project_id)
            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?
            .into_string();
        for field in ["title", "workspaceRoot", "faviconPath"] {
            if let Some(value) = command.fields.get(field).cloned() {
                if value.is_null() && field == "faviconPath" {
                    continue;
                }
                let value = value.as_str().ok_or_else(|| {
                    StoreError::InvalidCommand(format!("{field} must be a string"))
                })?;
                command.fields.insert(
                    field.into(),
                    json!(
                        t3_contracts::TrimmedNonEmptyString::new(value)
                            .map_err(|error| StoreError::InvalidCommand(error.to_string()))?
                            .as_str()
                    ),
                );
            }
        }
        if let Some(selection) = command
            .fields
            .get("defaultModelSelection")
            .cloned()
            .filter(|selection| !selection.is_null())
        {
            let selection: t3_contracts::ModelSelection = serde_json::from_value(selection)?;
            command.fields.insert(
                "defaultModelSelection".into(),
                serde_json::to_value(selection)?,
            );
        }
        Ok(command)
    }
}

#[derive(Clone)]
pub struct ProjectService {
    store: Store,
}
impl ProjectService {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    pub fn dispatch(
        &self,
        command: &ProjectCommand,
        now: DateTime<Utc>,
    ) -> Result<Receipt, StoreError> {
        let normalized = command.normalized()?;
        let command = &normalized;
        self.store.dispatch(
            &command.command_id,
            "project",
            &command.project_id,
            &command.command_type,
            now,
            |transaction| {
                let project = read_projection(transaction, "project", &command.project_id)?;
                let workspace_owner =
                    match command.fields.get("workspaceRoot").and_then(Value::as_str) {
                        Some(root) => read_projections(transaction, "project")?
                            .into_iter()
                            .find(|row| row["deletedAt"].is_null() && row["workspaceRoot"] == root),
                        None => None,
                    };
                Ok(
                    match plan_project_command(
                        command,
                        project.as_ref(),
                        workspace_owner.as_ref(),
                        &uuid::Uuid::new_v4().to_string(),
                        now,
                    ) {
                        Ok(event) => Decision::Accepted {
                            events: vec![event],
                            effects: Vec::new(),
                        },
                        Err(error) => Decision::Rejected(error),
                    },
                )
            },
            reduce_project_event,
        )
    }

    pub fn mutate(&self, input: Value, now: DateTime<Utc>) -> Result<Value, StoreError> {
        let mut command = ProjectCommand::from_json(input)?.normalized()?;
        if command.command_type == "project.update" {
            command.command_type = "project.meta.update".into();
        }
        if command.command_type == "project.delete"
            && self.store.projections("thread")?.iter().any(|projection| {
                projection["thread"]["projectId"] == command.project_id
                    && projection["thread"]["deletedAt"].is_null()
            })
        {
            return Err(StoreError::InvalidCommand(
                "Project has threads; native project cascade deletion is not yet ported.".into(),
            ));
        }
        if command.fields.get("createWorkspaceRootIfMissing") == Some(&json!(true)) {
            return Err(StoreError::InvalidCommand(
                "Native workspace creation is not yet ported.".into(),
            ));
        }
        let receipt = self.dispatch(&command, now)?;
        if receipt.status == "rejected" {
            return Err(StoreError::InvalidCommand(
                receipt.error.unwrap_or(Value::Null).to_string(),
            ));
        }
        let row = self
            .store
            .projection("project", &command.project_id)?
            .ok_or_else(|| StoreError::InvalidCommand("Committed project is missing.".into()))?;
        let deleted = row["deletedAt"].clone();
        let mut project = to_shell(row);
        project["deletedAt"] = deleted;
        Ok(project)
    }

    pub fn list(&self) -> Result<Vec<Value>, StoreError> {
        Ok(self
            .store
            .projections("project")?
            .into_iter()
            .filter(|row| row["deletedAt"].is_null())
            .map(to_shell)
            .collect())
    }
    pub fn get(&self, id: &str) -> Result<Option<Value>, StoreError> {
        Ok(self
            .store
            .projection("project", id)?
            .filter(|row| row["deletedAt"].is_null())
            .map(to_shell))
    }
}

pub fn plan_project_command(
    command: &ProjectCommand,
    project: Option<&Value>,
    workspace_owner: Option<&Value>,
    event_id: &str,
    now: DateTime<Utc>,
) -> Result<Event, Value> {
    let kind = command.command_type.as_str();
    let id = &command.project_id;
    let invariant = |detail: String| json!({"_tag":"ProjectCommandInvariantError","commandType":kind,"detail":detail});
    let missing =
        || json!({"_tag":"ProjectCommandMissingProjectError","commandType":kind,"projectId":id});
    let check_workspace = |root: &str| -> Result<(), Value> {
        if let Some(owner) = workspace_owner {
            if owner["projectId"] != *id {
                return Err(
                    json!({"_tag":"ProjectWorkspaceConflictError","workspaceRoot":root,"conflictingProjectId":owner["projectId"]}),
                );
            }
        }
        Ok(())
    };
    let required = |field: &str| -> Result<String, Value> {
        command
            .fields
            .get(field)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned)
            .ok_or_else(|| invariant(format!("{field} must be a non-empty string.")))
    };
    let at = now.to_rfc3339_opts(SecondsFormat::Millis, true);
    let (event_type, payload) = match kind {
        "project.create" => {
            if project.is_some() {
                return Err(invariant(format!(
                    "Project '{id}' already exists and cannot be created twice."
                )));
            }
            let title = required("title")?;
            let root = required("workspaceRoot")?;
            check_workspace(&root)?;
            let scripts = command
                .fields
                .get("scripts")
                .cloned()
                .unwrap_or_else(|| json!([]));
            (
                "project.created",
                json!({"projectId":id,"title":title,"workspaceRoot":root,"defaultModelSelection":null,"faviconPath":null,"projectIcon":null,"scripts":scripts,"createdAt":at,"updatedAt":at}),
            )
        }
        "project.meta.update" => {
            let project = project
                .filter(|row| row["deletedAt"].is_null())
                .ok_or_else(missing)?;
            if let Some(icon) = command.fields.get("projectIcon") {
                if icon["kind"] == "monogram"
                    && icon["text"]
                        .as_str()
                        .is_some_and(|text| text.graphemes(true).count() > 2)
                {
                    return Err(invariant(
                        "Project monograms must contain at most two characters.".into(),
                    ));
                }
            }
            if let Some(scripts) = command.fields.get("scripts") {
                let scripts = scripts
                    .as_array()
                    .ok_or_else(|| invariant("scripts must be an array.".into()))?;
                let existing: Vec<&str> = project["scripts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|script| script["id"].as_str())
                    .collect();
                for script in scripts {
                    let script_id = script["id"].as_str().unwrap_or("");
                    if !existing.contains(&script_id) && !valid_script_id(script_id) {
                        return Err(invariant(format!(
                            "Script IDs must be 1-24 lowercase letters, digits or hyphens, starting with a letter or digit (got {} characters).",
                            script_id.encode_utf16().count()
                        )));
                    }
                }
            }
            if let Some(root) = command.fields.get("workspaceRoot").and_then(Value::as_str) {
                check_workspace(root)?;
            }
            let mut payload = json!({"projectId":id,"updatedAt":at});
            for field in [
                "title",
                "workspaceRoot",
                "defaultModelSelection",
                "defaultThreadEnvMode",
                "autoPull",
                "faviconPath",
                "projectIcon",
                "scripts",
            ] {
                if let Some(value) = command.fields.get(field) {
                    payload[field] = value.clone();
                }
            }
            ("project.meta-updated", payload)
        }
        "project.delete" => {
            if project.filter(|row| row["deletedAt"].is_null()).is_none() {
                return Err(missing());
            }
            ("project.deleted", json!({"projectId":id,"deletedAt":at}))
        }
        _ => return Err(invariant(format!("Unknown project command '{kind}'."))),
    };
    Ok(Event {
        event_id: event_id.into(),
        aggregate_kind: "project".into(),
        aggregate_id: id.clone(),
        occurred_at: at,
        command_id: Some(command.command_id.clone()),
        causation_event_id: None,
        correlation_id: Some(command.command_id.clone()),
        event_type: event_type.into(),
        payload,
        metadata: json!({}),
    })
}

fn valid_script_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 24
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn reduce_project_event(
    transaction: &Transaction<'_>,
    stored: &StoredEvent,
) -> Result<(), StoreError> {
    let event = &stored.event;
    let id = &event.aggregate_id;
    let mut state = match event.event_type.as_str() {
        "project.created" => {
            let mut payload = event.payload.clone();
            payload["defaultThreadEnvMode"] = Value::Null;
            payload["autoPull"] = Value::Bool(false);
            payload["deletedAt"] = Value::Null;
            payload
        }
        "project.meta-updated" | "project.deleted" => read_projection(transaction, "project", id)?
            .ok_or_else(|| StoreError::InvalidProjection {
                kind: "project".into(),
                id: id.clone(),
                detail: "missing project for update".into(),
            })?,
        _ => {
            return Err(StoreError::InvalidProjection {
                kind: "project".into(),
                id: id.clone(),
                detail: format!("unknown event {}", event.event_type),
            });
        }
    };
    if event.event_type != "project.created" {
        if let Some(payload) = event.payload.as_object() {
            for (key, value) in payload {
                state[key] = value.clone();
            }
        }
    }
    write_projection(transaction, "project", id, &state)
}

pub fn to_shell(mut row: Value) -> Value {
    row["id"] = row["projectId"].clone();
    row["repositoryIdentity"] = Value::Null;
    if let Some(object) = row.as_object_mut() {
        object.remove("projectId");
        object.remove("deletedAt");
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> {
        "2026-01-01T00:00:00Z".parse().unwrap()
    }
    fn command(kind: &str, command_id: &str, fields: Value) -> ProjectCommand {
        ProjectCommand {
            command_type: kind.into(),
            command_id: command_id.into(),
            project_id: "project-scripts".into(),
            fields: serde_json::from_value(fields).unwrap(),
        }
    }
    fn row() -> Value {
        json!({"projectId":"project-scripts","title":"Scripts","workspaceRoot":"/tmp/scripts","scripts":[],"deletedAt":null})
    }
    fn create() -> ProjectCommand {
        command(
            "project.create",
            "create",
            json!({"title":"Scripts","workspaceRoot":"/tmp/scripts"}),
        )
    }

    #[test]
    fn creation_does_not_record_an_implicit_model_default() {
        let mut command = create();
        command.fields.insert(
            "defaultModelSelection".into(),
            json!({"instanceId":"codex","model":"gpt-5.6-sol"}),
        );
        let event = plan_project_command(&command, None, None, "event:planned", now()).unwrap();
        assert_eq!(event.payload["scripts"], json!([]));
        assert!(event.payload["defaultModelSelection"].is_null());
        assert_eq!(event.occurred_at, "2026-01-01T00:00:00.000Z");
    }

    #[test]
    fn edited_fields_preserve_explicit_null_and_omit_absent_fields() {
        let update = command(
            "project.meta.update",
            "update",
            json!({"title":"Renamed","defaultThreadEnvMode":null,"autoPull":true}),
        );
        let event = plan_project_command(&update, Some(&row()), None, "event", now()).unwrap();
        assert_eq!(event.payload["title"], "Renamed");
        assert!(event.payload.get("defaultThreadEnvMode").unwrap().is_null());
        assert!(event.payload.get("scripts").is_none());
    }

    #[test]
    fn legacy_script_ids_remain_editable_but_new_invalid_ids_are_rejected() {
        let invalid = [
            "install-javascript-dependencies",
            "A",
            "a.b",
            "a b",
            "-a",
            "aaaaaaaaaaaaaaaaaaaaaaaaa",
        ];
        for id in invalid {
            let update = command(
                "project.meta.update",
                "update",
                json!({"scripts":[{"id":id}]}),
            );
            let rejection =
                plan_project_command(&update, Some(&row()), None, "event", now()).unwrap_err();
            assert_eq!(rejection["_tag"], "ProjectCommandInvariantError");
            assert!(
                !rejection["detail"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("'{id}'"))
            );
            let mut old = row();
            old["scripts"] = json!([{"id":id}]);
            assert!(plan_project_command(&update, Some(&old), None, "event", now()).is_ok());
        }
    }

    #[test]
    fn monograms_count_unicode_graphemes() {
        for text in ["T3", "é", "किखि", "क्ष्म", "각"] {
            let update = command(
                "project.meta.update",
                "update",
                json!({"projectIcon":{"kind":"monogram","text":text}}),
            );
            assert!(
                plan_project_command(&update, Some(&row()), None, "event", now()).is_ok(),
                "{text}"
            );
        }
        for text in ["ABC", "किखिगि"] {
            let update = command(
                "project.meta.update",
                "update",
                json!({"projectIcon":{"kind":"monogram","text":text}}),
            );
            assert!(plan_project_command(&update, Some(&row()), None, "event", now()).is_err());
        }
    }

    #[test]
    fn receipts_replay_success_and_rejection_after_state_changes() {
        let store = Store::memory().unwrap();
        let service = ProjectService::new(store.clone());
        let missing = command(
            "project.meta.update",
            "rejected",
            json!({"title":"Before create"}),
        );
        let rejected = service.dispatch(&missing, now()).unwrap();
        assert_eq!(rejected.status, "rejected");
        let accepted = service.dispatch(&create(), now()).unwrap();
        assert_eq!(accepted.result_sequence, 1);
        assert_eq!(service.dispatch(&missing, now()).unwrap(), rejected);
        assert_eq!(service.dispatch(&create(), now()).unwrap(), accepted);
        assert_eq!(store.events(0, None, None, None, 100).unwrap().len(), 1);
    }

    #[test]
    fn deleted_ids_cannot_be_recreated_and_workspace_conflicts_are_durable() {
        let store = Store::memory().unwrap();
        let service = ProjectService::new(store);
        service.dispatch(&create(), now()).unwrap();
        let mut other = create();
        other.project_id = "other".into();
        other.command_id = "other-command".into();
        assert_eq!(
            service.dispatch(&other, now()).unwrap().error.unwrap()["_tag"],
            "ProjectWorkspaceConflictError"
        );
        service
            .dispatch(&command("project.delete", "delete", json!({})), now())
            .unwrap();
        let mut recreate = create();
        recreate.command_id = "recreate".into();
        assert_eq!(
            service.dispatch(&recreate, now()).unwrap().status,
            "rejected"
        );
        assert!(service.list().unwrap().is_empty());
    }
}
