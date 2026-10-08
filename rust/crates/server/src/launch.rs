//! Durable launch identity allocation and validation under the command transaction.
use crate::{
    persistence::{AcceptedCommand, Decision, Store, StoreError, read_projection},
    provider_registry::ProviderRegistry,
    thread,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

fn resolved_scripts(
    settings: &t3_contracts::ServerSettings,
    project_id: &t3_contracts::ProjectId,
    project: &Value,
) -> Result<Vec<Value>, StoreError> {
    let configured = if let Some(override_scripts) = settings
        .project_settings_overrides
        .get(project_id)
        .and_then(|settings| settings.default_project_scripts.as_ref())
    {
        Some(override_scripts)
    } else if settings.project_settings_folded {
        Some(&settings.default_project_scripts)
    } else {
        match settings.project_script_overrides.get(project_id) {
            Some(Some(scripts)) => Some(scripts),
            Some(None) => Some(&settings.default_project_scripts),
            None => None,
        }
    };
    if let Some(scripts) = configured {
        return Ok(serde_json::to_value(scripts)?.as_array().unwrap().clone());
    }
    if let Some(scripts) = project["scripts"]
        .as_array()
        .filter(|scripts| !scripts.is_empty())
    {
        return Ok(scripts.clone());
    }
    Ok(serde_json::to_value(&settings.default_project_scripts)?
        .as_array()
        .unwrap()
        .clone())
}

#[derive(Clone)]
pub struct ThreadLaunchService {
    store: Store,
    providers: Option<ProviderRegistry>,
}
impl ThreadLaunchService {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            providers: None,
        }
    }
    pub fn with_providers(store: Store, providers: ProviderRegistry) -> Self {
        Self {
            store,
            providers: Some(providers),
        }
    }
    pub fn launch(&self, input: Value, now: DateTime<Utc>) -> Result<Value, StoreError> {
        let decoded: t3_contracts::ThreadLaunchInput = serde_json::from_value(input)?;
        let input = decoded.service_payload()?;
        let command_id = input["commandId"].as_str().unwrap();
        let project_id = input["projectId"].as_str().unwrap();
        let project_key: t3_contracts::ProjectId = project_id.parse().expect("decoded project id");
        let defaults = t3_contracts::ServerSettings::default();
        let settings = self
            .providers
            .as_ref()
            .map(ProviderRegistry::settings)
            .unwrap_or(defaults);
        let supplied_id = input.get("threadId").and_then(Value::as_str);
        let reuse = input["reuseExistingThread"] == true;
        if reuse && supplied_id.is_none() {
            return Err(StoreError::InvalidCommand(
                "Reusing an existing thread requires a thread id.".into(),
            ));
        }
        if input.get("initialMessage").is_some() && input["generateTitle"] == true {
            return Err(StoreError::InvalidCommand(
                "Native initial-message title generation is not yet available.".into(),
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
                let mut events = thread::plan(&command,projection.as_ref(),project.as_ref(),now).map_err(|error|StoreError::InvalidCommand(error.to_string()))?;
                if resolved_scripts(&settings,&project_key,project.as_ref().unwrap())?.iter().any(|script|script["runOnWorktreeCreate"]==true) {return Err(StoreError::InvalidCommand("Native launch setup-script execution is not yet available.".into()));}
                let Some(initial)=input.get("initialMessage") else {return Ok(Decision::Accepted { events, effects: vec![] });};
                let providers=self.providers.as_ref().ok_or_else(||StoreError::InvalidCommand("Native provider execution is not configured.".into()))?;
                let driver=providers.driver(input["modelSelection"]["instanceId"].as_str().unwrap()).map_err(|error|StoreError::InvalidCommand(error.to_string()))?;
                let mut projected=projection;
                for event in &events {projected=Some(thread::projection_after(projected,event)?);}
                let initial_id=format!("{command_id}:initial-message");let release_id=format!("{command_id}:release");
                let mut message=json!({"type":"message.dispatch","commandId":initial_id,"threadId":id,"messageId":initial.get("messageId").cloned().unwrap_or_else(||json!(uuid::Uuid::new_v4().to_string())),"text":initial["text"],"attachments":initial["attachments"],"modelSelection":input["modelSelection"],"createdBy":"user","creationSource":input.get("creationSource").cloned().unwrap_or(json!("web")),"dispatchMode":{"type":"defer_start","workspaceStrategy":strategy}});
                if let Some(context)=initial.get("context"){message["context"]=context.clone();}
                let message:t3_contracts::ProviderCommand=serde_json::from_value(message)?;let message=message.service_payload()?;
                let decision=crate::execution::plan_message_for_driver(&message,projected.as_ref().unwrap(),now,driver)?;
                let Decision::Accepted{events:message_events,effects:message_effects}=decision else {return Err(StoreError::InvalidCommand("Initial-message planning did not accept.".into()));};
                if !message_effects.is_empty(){return Err(StoreError::InvalidCommand("Prepared initial message scheduled provider work before release.".into()));}
                let run_id=message_events.iter().find(|event|event.event_type=="run.created").unwrap().payload["id"].clone();
                for event in &message_events {projected=Some(thread::projection_after(projected,event)?);}events.extend(message_events);
                let release=json!({"type":"prepared-run.release","commandId":release_id,"threadId":id,"runId":run_id});
                let Decision::Accepted{events:release_events,effects}=crate::execution::plan_release(&release,projected.as_ref().unwrap(),now)? else{return Err(StoreError::InvalidCommand("Prepared initial-message release did not accept.".into()));};
                events.extend(release_events);
                Ok(Decision::AcceptedBatch{events,effects,commands:vec![AcceptedCommand{command_id:initial_id,command_type:"message.dispatch".into()},AcceptedCommand{command_id:release_id,command_type:"prepared-run.release".into()}]})
            }, thread::reduce)?;
        let projection = self
            .store
            .projection("thread", &receipt.aggregate_id)?
            .ok_or_else(|| StoreError::InvalidCommand("Launched thread was removed.".into()))?;
        crate::execution::checked::<t3_contracts::ThreadLaunchResult>(
            json!({"threadId":receipt.aggregate_id,"projection":crate::wire_projection::projection(&projection),"resumed":resumed}),
        )
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
    async fn registry(
        directory: &std::path::Path,
        record: Option<&std::path::Path>,
    ) -> ProviderRegistry {
        let binary = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex-provider.py");
        let mut entry = json!({"driver":"codex","config":{"binaryPath":binary}});
        if let Some(record) = record {
            entry["environment"] = json!([{"name":"FIXTURE_RPC_RECORD","value":record}]);
        }
        let settings: t3_contracts::ServerSettings =
            serde_json::from_value(json!({"providerInstances":{"codex":entry}})).unwrap();
        ProviderRegistry::discover(&settings, directory)
            .await
            .unwrap()
    }
    fn initial() -> Value {
        let mut input = input();
        input["initialMessage"] = json!({"text":"Initial prompt","attachments":[]});
        input
    }
    #[test]
    fn setup_script_resolution_honors_folded_and_legacy_overrides_without_skipping_required_setup()
    {
        let script = |id: &str| json!({"id":id,"name":"Setup","command":"echo setup","icon":"play","runOnWorktreeCreate":true});
        let project = json!({"scripts":[script("project")]});
        let project_id: t3_contracts::ProjectId = "project".parse().unwrap();
        let mut settings: t3_contracts::ServerSettings =
            serde_json::from_value(json!({"defaultProjectScripts":[script("default")]})).unwrap();
        assert_eq!(
            resolved_scripts(&settings, &project_id, &project).unwrap()[0]["id"],
            "project"
        );
        settings.project_settings_folded = true;
        assert_eq!(
            resolved_scripts(&settings, &project_id, &project).unwrap()[0]["id"],
            "default"
        );
        settings.project_settings_folded = false;
        settings
            .project_script_overrides
            .insert(project_id.clone(), None);
        assert_eq!(
            resolved_scripts(&settings, &project_id, &project).unwrap()[0]["id"],
            "default"
        );
        settings.project_settings_overrides.insert(
            project_id.clone(),
            serde_json::from_value(json!({"defaultProjectScripts":[]})).unwrap(),
        );
        assert!(
            resolved_scripts(&settings, &project_id, &project)
                .unwrap()
                .is_empty()
        );
        let directory = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        setup(&store, directory.path());
        crate::project::ProjectService::new(store.clone()).mutate(json!({"type":"project.update","commandId":"scripts","projectId":"project","scripts":[script("project")]}),Utc::now()).unwrap();
        assert!(
            ThreadLaunchService::new(store.clone())
                .launch(input(), Utc::now())
                .is_err()
        );
        assert!(store.projections("thread").unwrap().is_empty());
    }
    #[tokio::test]
    async fn atomic_initial_message_allocates_once_and_rolls_back_invalid_or_colliding_derived_commands()
     {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let store = Store::open(&path).unwrap();
        setup(&store, directory.path());
        let providers = registry(directory.path(), None).await;
        let service = ThreadLaunchService::with_providers(store.clone(), providers.clone());
        let mut invalid = initial();
        invalid["initialMessage"]["text"] = json!(42);
        assert!(service.launch(invalid, Utc::now()).is_err());
        assert!(store.receipt("launch").unwrap().is_none());
        assert!(store.projections("thread").unwrap().is_empty());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles = [Store::open(&path).unwrap(), Store::open(&path).unwrap()].map(|store| {
            let providers = providers.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ThreadLaunchService::with_providers(store, providers)
                    .launch(initial(), Utc::now())
                    .unwrap()
            })
        });
        let [a, b] = handles.map(|handle| handle.join().unwrap());
        assert_eq!(a["threadId"], b["threadId"]);
        assert_ne!(a["resumed"], b["resumed"]);
        let id = a["threadId"].as_str().unwrap();
        let projection = store.projection("thread", id).unwrap().unwrap();
        assert_eq!(projection["runs"].as_array().unwrap().len(), 1);
        assert_eq!(projection["messages"].as_array().unwrap().len(), 1);
        assert_eq!(projection["runs"][0]["status"], "starting");
        assert_eq!(projection["turnItems"][1]["status"], "completed");
        assert_eq!(
            projection["turnItems"][1]["output"],
            "Workspace preparation completed."
        );
        let count: u64 = store
            .read(|connection| {
                Ok(
                    connection.query_row("SELECT COUNT(*) FROM rust_effect_outbox", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(count, 1);
        let receipt = store.receipt("launch:initial-message").unwrap().unwrap();
        assert!(
            store.receipt("launch").unwrap().unwrap().result_sequence < receipt.result_sequence
        );
        assert_eq!(receipt.command_type, "message.dispatch");
        let release = store.receipt("launch:release").unwrap().unwrap();
        assert_eq!(release.command_type, "prepared-run.release");
        assert!(receipt.result_sequence < release.result_sequence);
        let reopened =
            ThreadLaunchService::with_providers(Store::open(&path).unwrap(), providers.clone())
                .launch(initial(), Utc::now())
                .unwrap();
        assert_eq!(reopened["threadId"], id);
        assert_eq!(reopened["resumed"], true);
        let mut empty = input();
        empty["commandId"] = json!("other");
        let other = service.launch(empty, Utc::now()).unwrap();
        crate::thread::ThreadService::new(store.clone()).dispatch(&json!({"type":"thread.metadata.update","commandId":"collision:initial-message","threadId":other["threadId"],"title":"Other"}),Utc::now()).unwrap();
        let before = store.latest_sequence().unwrap();
        let mut collision = initial();
        collision["commandId"] = json!("collision");
        collision["threadId"] = json!("colliding-thread");
        assert!(service.launch(collision, Utc::now()).is_err());
        assert_eq!(store.latest_sequence().unwrap(), before);
        assert!(
            store
                .projection("thread", "colliding-thread")
                .unwrap()
                .is_none()
        );
        assert!(store.receipt("collision").unwrap().is_none());
    }
    #[tokio::test]
    async fn reused_empty_thread_initial_message_keeps_empty_text_and_overrides_model_options() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        setup(&store, directory.path());
        let providers = registry(directory.path(), None).await;
        let service = ThreadLaunchService::with_providers(store.clone(), providers);
        let mut empty = input();
        empty["commandId"] = json!("empty");
        let id = service.launch(empty, Utc::now()).unwrap()["threadId"].clone();
        let mut launch = initial();
        launch["threadId"] = id.clone();
        launch["reuseExistingThread"] = json!(true);
        launch["initialMessage"]["text"] = json!("");
        launch["modelSelection"] = json!({"instanceId":"codex","model":"fixture-second","options":[{"id":"reasoningEffort","value":"low"}]});
        let selected = launch["modelSelection"].clone();
        service.launch(launch, Utc::now()).unwrap();
        let projection = store
            .projection("thread", id.as_str().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(projection["runs"][0]["modelSelection"], selected);
        assert_eq!(projection["thread"]["modelSelection"], selected);
        assert_eq!(projection["messages"][0]["text"], "");
        assert_eq!(projection["turnItems"][0]["text"], "");
    }
    async fn completed(
        store: &Store,
        events: &mut tokio::sync::broadcast::Receiver<Vec<crate::persistence::StoredEvent>>,
        id: &str,
        count: usize,
    ) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let projection = store.projection("thread", id).unwrap().unwrap();
                if projection["runs"].as_array().unwrap().len() == count
                    && projection["runs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|run| run["status"] == "completed")
                {
                    return projection;
                }
                events.recv().await.unwrap();
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn initial_launch_and_persisted_next_turn_choices_reach_the_real_owned_provider() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("state.db")).unwrap();
        setup(&store, directory.path());
        let record = directory.path().join("rpc.jsonl");
        let providers = registry(directory.path(), Some(&record)).await;
        let execution = crate::execution::ExecutionService::start(store.clone(), providers.clone());
        let service = ThreadLaunchService::with_providers(store.clone(), providers);
        let mut events = store.subscribe();
        let mut initial = initial();
        initial["modelSelection"]["options"] =
            json!([{"id":"reasoningEffort","value":"low"},{"id":"serviceTier","value":"fast"}]);
        let result = service.launch(initial.clone(), Utc::now()).unwrap();
        let id = result["threadId"].as_str().unwrap();
        let projection = completed(&store, &mut events, id, 1).await;
        assert_eq!(projection["messages"][0]["text"], "Initial prompt");
        assert!(
            projection["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "command_execution"
                    && item["input"] != "Preparing workspace"
                    && item["output"] == "fixture tool output\n")
        );
        let before = store.latest_sequence().unwrap();
        assert_eq!(
            service.launch(initial, Utc::now()).unwrap()["resumed"],
            true
        );
        assert_eq!(store.latest_sequence().unwrap(), before);
        let original = store.receipt("launch:initial-message").unwrap().unwrap();
        assert_eq!(execution.dispatch(&json!({"type":"message.dispatch","commandId":"launch:initial-message","threadId":id,"messageId":"another-message","text":"Should not execute","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}}),Utc::now()).unwrap(),original);
        let threads = crate::thread::ThreadService::new(store.clone());
        assert_eq!(threads.dispatch(&json!({"type":"thread.runtime-mode.set","commandId":"mode","threadId":id,"runtimeMode":"full-access"}),Utc::now()).unwrap().status,"accepted");
        assert_eq!(threads.dispatch(&json!({"type":"thread.model-selection.set","commandId":"model","threadId":id,"modelSelection":{"instanceId":"codex","model":"fixture-second","options":[{"id":"reasoningEffort","value":"medium"},{"id":"serviceTier","value":"flex"}]}}),Utc::now()).unwrap().status,"accepted");
        execution.dispatch(&json!({"type":"message.dispatch","commandId":"second","threadId":id,"messageId":"second-message","text":"Second prompt","attachments":[],"createdBy":"user","creationSource":"web","dispatchMode":{"type":"start_immediately"}}),Utc::now()).unwrap();
        completed(&store, &mut events, id, 2).await;
        let records: Vec<Value> = std::fs::read_to_string(&record)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let turns: Vec<_> = records
            .iter()
            .filter(|record| record["method"] == "turn/start")
            .collect();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0]["params"]["model"], "fixture-model");
        assert_eq!(turns[0]["params"]["effort"], "low");
        assert_eq!(turns[0]["params"]["serviceTier"], "fast");
        assert_eq!(turns[0]["params"]["approvalPolicy"], "untrusted");
        assert_eq!(turns[0]["params"]["sandboxPolicy"]["type"], "readOnly");
        assert_eq!(turns[1]["params"]["model"], "fixture-second");
        assert_eq!(turns[1]["params"]["effort"], "medium");
        assert_eq!(turns[1]["params"]["serviceTier"], "flex");
        assert_eq!(turns[1]["params"]["approvalPolicy"], "never");
        assert_eq!(
            turns[1]["params"]["sandboxPolicy"]["type"],
            "dangerFullAccess"
        );
        assert_eq!(
            records
                .iter()
                .filter(|record| record["method"] == "thread/start")
                .count(),
            1
        );
        execution.shutdown().await;
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
