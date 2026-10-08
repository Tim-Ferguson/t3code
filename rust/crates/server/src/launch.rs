//! Durable launch identity allocation and validation under the command transaction.
use crate::{
    persistence::{Decision, Store, StoreError, read_projection},
    thread,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct ThreadLaunchService {
    store: Store,
}
impl ThreadLaunchService {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
    pub fn launch(&self, input: Value, now: DateTime<Utc>) -> Result<Value, StoreError> {
        let decoded: t3_contracts::ThreadLaunchInput = serde_json::from_value(input)?;
        let input = decoded.service_payload()?;
        let command_id = input["commandId"].as_str().unwrap();
        let project_id = input["projectId"].as_str().unwrap();
        let supplied_id = input.get("threadId").and_then(Value::as_str);
        let reuse = input["reuseExistingThread"] == true;
        if reuse && supplied_id.is_none() {
            return Err(StoreError::InvalidCommand(
                "Reusing an existing thread requires a thread id.".into(),
            ));
        }
        if input.get("initialMessage").is_some() {
            return Err(StoreError::InvalidCommand(
                "Native launch initial-message execution is not yet available.".into(),
            ));
        }
        let strategy = &input["workspaceStrategy"];
        if strategy["type"] == "worktree" {
            return Err(StoreError::InvalidCommand(
                "Native Git worktree preparation is not yet available.".into(),
            ));
        }
        let command_type = if reuse {
            "thread.metadata.update"
        } else {
            "thread.create"
        };
        let (receipt, resumed) = self.store.dispatch_resolved(command_id, "thread", command_type, now,
            |transaction, receipt| {
                // Validation precedes receipt insertion, so an invalid concurrent caller
                // cannot reserve the shared launch command id away from a valid caller.
                read_projection(transaction, "project", project_id)?.filter(|project| project["deletedAt"].is_null())
                    .ok_or_else(|| StoreError::InvalidCommand("Project not found.".into()))?;
                let id = match receipt {
                    Some(receipt) => {
                        if receipt.status != "accepted" || receipt.aggregate_kind != "thread" || receipt.command_type != command_type {
                            return Err(StoreError::InvalidCommand("Command ID does not belong to an accepted thread launch.".into()));
                        }
                        if supplied_id.is_some_and(|id| id != receipt.aggregate_id) {
                            return Err(StoreError::InvalidCommand("Supplied thread id differs from the recorded launch.".into()));
                        }
                        receipt.aggregate_id.clone()
                    }
                    None => supplied_id.map(ToOwned::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                };
                let projection = read_projection(transaction, "thread", &id)?;
                if receipt.is_some() || reuse {
                    let projection = projection.ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
                    if projection["thread"]["projectId"] != project_id || !projection["thread"]["deletedAt"].is_null() {
                        return Err(StoreError::InvalidCommand("Thread is deleted or belongs to another project.".into()));
                    }
                    if receipt.is_none() && (!projection["thread"]["archivedAt"].is_null() || !projection["runs"].as_array().is_some_and(Vec::is_empty) || !projection["messages"].as_array().is_some_and(Vec::is_empty)) {
                        return Err(StoreError::InvalidCommand("Only an empty active thread in the target project can change workspace during launch.".into()));
                    }
                } else if projection.is_some() { return Err(StoreError::InvalidCommand("Thread already exists.".into())); }
                Ok(id)
            },
            |transaction, id| {
                let command = if reuse { json!({"type":"thread.metadata.update","commandId":command_id,"threadId":id,"expectedEmpty":true}) }
                    else { json!({"type":"thread.create","commandId":command_id,"threadId":id,"projectId":project_id,"title":input["title"],"modelSelection":input["modelSelection"],"runtimeMode":input["runtimeMode"],"interactionMode":input["interactionMode"],"createdBy":"user","creationSource":input.get("creationSource").cloned().unwrap_or(json!("web")),"branch":strategy.get("branch").cloned().unwrap_or(Value::Null),"worktreePath":if strategy["type"] == "existing_worktree" { strategy["worktreePath"].clone() } else { Value::Null }}) };
                let projection = read_projection(transaction,"thread",id)?;
                let project = read_projection(transaction,"project",project_id)?;
                let events = thread::plan(&command,projection.as_ref(),project.as_ref(),now).map_err(|error|StoreError::InvalidCommand(error.to_string()))?;
                Ok(Decision::Accepted { events, effects: vec![] })
            }, thread::reduce)?;
        let projection = self
            .store
            .projection("thread", &receipt.aggregate_id)?
            .ok_or_else(|| StoreError::InvalidCommand("Launched thread was removed.".into()))?;
        Ok(json!({"threadId":receipt.aggregate_id,"projection":projection,"resumed":resumed}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::ProjectService;
    fn setup(store: &Store, path: &std::path::Path) {
        ProjectService::new(store.clone()).mutate(json!({"type":"project.create","commandId":"create-project","projectId":"project","title":"Project","workspaceRoot":path}), Utc::now()).unwrap();
    }
    fn input() -> Value {
        json!({"commandId":"launch","projectId":"project","title":"Thread","modelSelection":{"instanceId":"codex","model":"fixture-model"},"runtimeMode":"approval-required","interactionMode":"default","workspaceStrategy":{"type":"root"}})
    }
    #[test]
    fn allocated_identity_survives_reopen_and_rejects_mismatch_or_deleted_target() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let store = Store::open(&path).unwrap();
        setup(&store, directory.path());
        let result = ThreadLaunchService::new(store.clone())
            .launch(input(), Utc::now())
            .unwrap();
        assert_eq!(result["resumed"], false);
        let id = result["threadId"].as_str().unwrap();
        let reopened = Store::open(&path).unwrap();
        let replay = ThreadLaunchService::new(reopened.clone())
            .launch(input(), Utc::now())
            .unwrap();
        assert_eq!(replay["threadId"], id);
        assert_eq!(replay["resumed"], true);
        let mut mismatch = input();
        mismatch["threadId"] = json!("other");
        assert!(
            ThreadLaunchService::new(reopened.clone())
                .launch(mismatch, Utc::now())
                .is_err()
        );
        thread::ThreadService::new(reopened.clone())
            .dispatch(
                &json!({"type":"thread.delete","commandId":"delete","threadId":id}),
                Utc::now(),
            )
            .unwrap();
        assert!(
            ThreadLaunchService::new(reopened)
                .launch(input(), Utc::now())
                .is_err()
        );
    }
    #[test]
    fn concurrent_independent_connections_allocate_one_thread_and_invalid_peer_cannot_claim() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let store = Store::open(&path).unwrap();
        setup(&store, directory.path());
        let bad = Store::open(&path).unwrap();
        let mut invalid = input();
        invalid["reuseExistingThread"] = json!(true);
        invalid["threadId"] = json!("missing");
        assert!(
            ThreadLaunchService::new(bad)
                .launch(invalid, Utc::now())
                .is_err()
        );
        let a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = [a, b].map(|store| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ThreadLaunchService::new(store)
                    .launch(input(), Utc::now())
                    .unwrap()
            })
        });
        let [a, b] = handles.map(|handle| handle.join().unwrap());
        assert_eq!(a["threadId"], b["threadId"]);
        assert_ne!(a["resumed"], b["resumed"]);
        assert_eq!(
            store
                .events(0, None, Some("thread"), None, 100)
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn unrelated_receipt_and_changed_project_do_not_replay_as_launch() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        setup(&store, directory.path());
        let service = ThreadLaunchService::new(store.clone());
        let result = service.launch(input(), Utc::now()).unwrap();
        let id = result["threadId"].as_str().unwrap();
        thread::ThreadService::new(store.clone()).dispatch(&json!({"type":"thread.metadata.update","commandId":"metadata","threadId":id,"title":"Changed"}),Utc::now()).unwrap();
        let mut wrong = input();
        wrong["commandId"] = json!("metadata");
        assert!(service.launch(wrong, Utc::now()).is_err());
        ProjectService::new(store).mutate(json!({"type":"project.create","commandId":"other-project","projectId":"other","title":"Other","workspaceRoot":directory.path().join("other")}),Utc::now()).unwrap();
        let mut wrong = input();
        wrong["projectId"] = json!("other");
        assert!(service.launch(wrong, Utc::now()).is_err());
    }
}
