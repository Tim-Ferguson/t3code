//! Owned Codex runtime actors. Native callbacks enter the same persisted reducer
//! as user commands; subprocess callbacks never mutate UI state directly.
use crate::{
    codex::{CodexConnection, CodexInstance},
    execution::{at, event},
    persistence::{Decision, Effect, Store, StoreError, read_projection},
    provider_process::{ProcessError, ProcessEvent},
    provider_registry::ProviderRegistry,
    thread,
};
use chrono::Utc;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};

#[derive(Clone)]
pub(crate) struct ActorLifetime {
    instance: Arc<Mutex<Option<String>>>,
    stop: watch::Sender<bool>,
    done: watch::Sender<bool>,
}
impl ActorLifetime {
    fn new(instance: Option<String>) -> Self {
        Self {
            instance: Arc::new(Mutex::new(instance)),
            stop: watch::channel(false).0,
            done: watch::channel(false).0,
        }
    }
    #[cfg(test)]
    pub(crate) async fn wait_stopped(&self) {
        let mut stopped = self.stop.subscribe();
        let _ = stopped.wait_for(|stopped| *stopped).await;
    }
    pub(crate) fn captured_instance(&self) -> Option<String> {
        self.instance.lock().unwrap().clone()
    }
    pub(crate) async fn stop_and_wait(&self) {
        self.stop.send_replace(true);
        self.wait_finished().await;
    }
    async fn wait_finished(&self) {
        let mut done = self.done.subscribe();
        while !*done.borrow_and_update() {
            if done.changed().await.is_err() {
                break;
            }
        }
    }
}
struct ActorFinished(ActorLifetime);
impl Drop for ActorFinished {
    fn drop(&mut self) {
        self.0.done.send_replace(true);
    }
}

pub struct RuntimeOwner {
    stop: watch::Sender<bool>,
    wake: Arc<tokio::sync::Notify>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    cancellations: Arc<Mutex<HashMap<String, watch::Sender<Option<String>>>>>,
    actors: Arc<Mutex<HashMap<String, ActorLifetime>>>,
    #[cfg(test)]
    pause_actor: Arc<Mutex<Option<mpsc::UnboundedSender<(ActorLifetime, oneshot::Sender<()>)>>>>,
}
impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
impl RuntimeOwner {
    pub(crate) async fn wait_instances(&self, instances: &[String]) {
        let actors = self
            .actors
            .lock()
            .unwrap()
            .values()
            .filter(|actor| {
                actor
                    .captured_instance()
                    .as_ref()
                    .is_some_and(|instance| instances.contains(instance))
            })
            .cloned()
            .collect::<Vec<_>>();
        futures_util::future::join_all(actors.iter().map(ActorLifetime::wait_finished)).await;
    }
    #[cfg(test)]
    pub(crate) fn cancellation_target(&self, thread_id: &str) -> Option<String> {
        self.cancellations
            .lock()
            .unwrap()
            .get(thread_id)
            .and_then(|sender| sender.borrow().clone())
    }
    pub fn cancel_start(&self, thread_id: &str, run_id: &str) {
        if let Some(sender) = self.cancellations.lock().unwrap().get(thread_id) {
            let _ = sender.send(Some(run_id.into()));
        }
    }
    #[cfg(test)]
    pub(crate) fn pause_next_actor(
        &self,
    ) -> mpsc::UnboundedReceiver<(ActorLifetime, oneshot::Sender<()>)> {
        let (sender, receiver) = mpsc::unbounded_channel();
        *self.pause_actor.lock().unwrap() = Some(sender);
        receiver
    }
    pub async fn stop_instances(&self, instances: &[String]) {
        let actors = self
            .actors
            .lock()
            .unwrap()
            .values()
            .filter(|actor| {
                actor
                    .instance
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|instance| instances.contains(instance))
            })
            .cloned()
            .collect::<Vec<_>>();
        futures_util::future::join_all(actors.iter().map(ActorLifetime::stop_and_wait)).await;
    }
    pub fn wake(&self) {
        self.wake.notify_one();
    }
    pub async fn shutdown(&self) {
        let _ = self.stop.send(true);
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}
pub(crate) struct Work {
    pub(crate) effect: Effect,
    pub(crate) complete: oneshot::Sender<Result<(), String>>,
}
pub fn start(
    store: Store,
    providers: ProviderRegistry,
    lease: crate::persistence::RuntimeLease,
) -> Arc<RuntimeOwner> {
    let (stop, mut stopped) = watch::channel(false);
    let wake = Arc::new(tokio::sync::Notify::new());
    let notified = wake.clone();
    let mut committed = store.subscribe();
    let cancellations = Arc::new(Mutex::new(HashMap::new()));
    let actor_cancellations = cancellations.clone();
    let controls = Arc::new(Mutex::new(HashMap::<String, ActorLifetime>::new()));
    let actor_controls = controls.clone();
    #[cfg(test)]
    let pause_actor = Arc::new(Mutex::new(
        None::<mpsc::UnboundedSender<(ActorLifetime, oneshot::Sender<()>)>>,
    ));
    #[cfg(test)]
    let paused = pause_actor.clone();
    let task = tokio::spawn(async move {
        let _lease = lease;
        let owner = uuid::Uuid::new_v4().to_string();
        let mut actors: HashMap<String, mpsc::Sender<Work>> = HashMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        let mut effects = tokio::task::JoinSet::new();
        let mut scan = tokio::time::interval(Duration::from_secs(1));
        loop {
            if *stopped.borrow() {
                break;
            }
            let claimed = if effects.len() < 32 {
                store.claim_effect(&owner, Utc::now(), chrono::Duration::seconds(60))
            } else {
                Ok(None)
            };
            match claimed {
                Ok(Some(effect)) => {
                    let (complete, result) = oneshot::channel();
                    if !actors
                        .get(&effect.thread_id)
                        .is_some_and(|actor| !actor.is_closed())
                    {
                        let (send, receive) = mpsc::channel(64);
                        let (cancel, canceled) = watch::channel(None);
                        actor_cancellations
                            .lock()
                            .unwrap()
                            .insert(effect.thread_id.clone(), cancel);
                        actors.insert(effect.thread_id.clone(), send);
                        let instance =
                            projection(&store, &effect.thread_id).ok().and_then(|view| {
                                view["thread"]["modelSelection"]["instanceId"]
                                    .as_str()
                                    .map(ToOwned::to_owned)
                            });
                        let lifetime = ActorLifetime::new(instance);
                        let actor_id = uuid::Uuid::new_v4().to_string();
                        actor_controls
                            .lock()
                            .unwrap()
                            .insert(actor_id.clone(), lifetime.clone());
                        let captured_controls = actor_controls.clone();
                        let captured_store = store.clone();
                        let captured_providers = providers.clone();
                        let captured_thread = effect.thread_id.clone();
                        #[cfg(test)]
                        let pause = paused.lock().unwrap().take();
                        tasks.spawn(async move {
                            #[cfg(test)]
                            if let Some(pause) = pause {
                                let (release, released) = oneshot::channel();
                                if pause.send((lifetime.clone(), release)).is_ok() {
                                    let _ = released.await;
                                }
                            }
                            actor(
                                captured_store,
                                captured_providers,
                                captured_thread,
                                receive,
                                lifetime,
                                canceled,
                            )
                            .await;
                            captured_controls.lock().unwrap().remove(&actor_id);
                        });
                    }
                    let work = Work {
                        effect: effect.clone(),
                        complete,
                    };
                    let actor = actors[&effect.thread_id].clone();
                    let store = store.clone();
                    let owner = owner.clone();
                    let mut stopped = stopped.clone();
                    effects.spawn(async move{
                    let result=tokio::select!{result=async{if actor.send(work).await.is_err(){Err("Provider actor stopped.".into())}else{result.await.unwrap_or_else(|_|Err("Provider actor stopped.".into()))}}=>result,_=stopped.changed()=>Err("The server stopped during provider execution.".into())};
                    if let Err(error) = &result {
                        if let Err(settlement)=fail_run(
                            &store,
                            &effect.thread_id,
                            effect.request["runId"].as_str(),
                            error,
                        ){tracing::error!(%settlement, "Failed to persist provider failure");}
                    }
                    if let Err(settlement)=store.finish_effect(
                        &effect.id,
                        &owner,
                        Utc::now(),
                        result.as_ref().map(|_| ()).map_err(String::as_str),
                        None,
                    ){if matches!(settlement,StoreError::LeaseLost(_)){tracing::debug!(%settlement,"Provider effect was cancelled by a terminal run");}else{tracing::error!(%settlement,"Failed to settle provider effect");}}
                    });
                    continue;
                }
                Ok(None) => {}
                Err(error) => tracing::error!(%error,"Native effect claim failed"),
            }
            tokio::select! { _=stopped.changed()=>{}, _=notified.notified()=>{}, _=committed.recv()=>{}, _=scan.tick()=>{}, result=effects.join_next(),if !effects.is_empty()=>{if let Some(Err(error))=result{tracing::error!(%error,"Provider effect failed");}}, result=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Err(error))=result{tracing::error!(%error,"Provider actor failed");}} }
        }
        for actor in actor_controls.lock().unwrap().values() {
            actor.stop.send_replace(true);
        }
        drop(actors);
        while effects.join_next().await.is_some() {}
        while tasks.join_next().await.is_some() {}
    });
    Arc::new(RuntimeOwner {
        stop,
        wake,
        task: Mutex::new(Some(task)),
        cancellations,
        actors: controls,
        #[cfg(test)]
        pause_actor,
    })
}

struct Actor {
    store: Store,
    thread_id: String,
    session_id: String,
    connection: CodexConnection,
    instance_id: String,
    active_run: Option<String>,
    native_thread_id: Option<String>,
    native_turn_id: Option<String>,
    callbacks: HashMap<String, Value>,
    mcp: Option<crate::mcp_sessions::ProviderSessionConfig>,
}
async fn actor(
    store: Store,
    providers: ProviderRegistry,
    thread_id: String,
    mut work: mpsc::Receiver<Work>,
    lifetime: ActorLifetime,
    canceled: watch::Receiver<Option<String>>,
) {
    let instance_id = lifetime.captured_instance();
    let _finished = ActorFinished(lifetime.clone());
    let mut stopped = lifetime.stop.subscribe();
    if *stopped.borrow() {
        return;
    }
    let mcp = match instance_id
        .as_deref()
        .map(|instance| providers.reserve_mcp(&store, &thread_id, instance))
        .transpose()
    {
        Ok(credential) => credential.flatten(),
        Err(error) => {
            tokio::select! {biased; _=stopped.wait_for(|stop|*stop)=>{}, incoming=work.recv()=>{if let Some(work)=incoming {let _=work.complete.send(Err(error));}}}
            return;
        }
    };
    if instance_id
        .as_deref()
        .and_then(|id| providers.driver(id).ok())
        == Some("acpRegistry")
    {
        crate::acp_adapter::actor(
            store,
            providers,
            thread_id,
            work,
            stopped,
            canceled,
            lifetime,
            mcp.as_ref(),
        )
        .await;
    } else {
        codex_actor(
            store,
            providers,
            thread_id,
            work,
            stopped,
            canceled,
            mcp.as_ref(),
        )
        .await;
    }
}
async fn codex_actor(
    store: Store,
    providers: ProviderRegistry,
    thread_id: String,
    mut work: mpsc::Receiver<Work>,
    mut stopped: watch::Receiver<bool>,
    mut canceled: watch::Receiver<Option<String>>,
    mcp: Option<&crate::provider_mcp::CredentialLease>,
) {
    let mut runtime: Option<Actor> = None;
    let mut pending_process = None;
    let mut user_cancelled = false;
    loop {
        if *stopped.borrow() {
            break;
        }
        tokio::select! {
            _=stopped.changed()=>{break;},
            incoming=work.recv()=>{
                let Some(incoming)=incoming else{break};
                let starting=incoming.effect.request["type"]=="provider-turn.start";
                let run_id=incoming.effect.request["runId"].as_str();
                let result=tokio::select! {result=async {
                    if starting {if let Some(mcp)=mcp {mcp.touch();}}
                    if runtime.is_none(){runtime=Some(Actor::connect(store.clone(),&providers,&thread_id,&mut pending_process,mcp).await?);pending_process.take();}
                    runtime.as_mut().unwrap().effect(&incoming.effect).await
                }=>result,_=stopped.changed()=>{break;},_=async{loop{if canceled.borrow_and_update().as_deref()==run_id{break;}if canceled.changed().await.is_err(){std::future::pending::<()>().await;}}},if starting=>{user_cancelled=true;Err("The run was interrupted during provider startup.".into())}};
                let close=starting && result.is_err();
                if user_cancelled||close{work.close();}
                let _=incoming.complete.send(result);
                if user_cancelled||close{break;}
            },
            incoming=async{runtime.as_mut().unwrap().connection.events.recv().await},if runtime.is_some()=>{
                let runtime=runtime.as_mut().unwrap();
                match incoming {
                    Ok(ProcessEvent::IngressBarrier{acknowledgement})=>{acknowledgement.acknowledge();},
                    Ok(ProcessEvent::Notification{method,params})=>{if let Err(error)=runtime.notification(&method,&params){work.close();report(fail_run(&runtime.store,&runtime.thread_id,runtime.active_run.as_deref(),&error.to_string()),"provider failure");break;}},
                    Ok(ProcessEvent::Request{id,method,params})=>{if let Err(error)=runtime.request(id.clone(),&method,&params){let _=runtime.connection.process.respond(id,Err(ProcessError::Protocol(error.to_string()))).await;}},
                    Ok(ProcessEvent::Closed(error))=>{work.close();report(fail_run(&runtime.store,&runtime.thread_id,runtime.active_run.as_deref(),&error.to_string()),"provider failure");break;},
                    Err(error)=>{work.close();report(fail_run(&runtime.store,&runtime.thread_id,runtime.active_run.as_deref(),&format!("Provider event continuity lost: {error}")),"provider failure");break;},
                }
            }
        }
    }
    // Publish the actor's stopped session only after its command receiver has
    // closed. A new run must create a fresh actor instead of entering a mailbox
    // that the old actor will never drain while its subprocess is being reaped.
    work.close();
    if let Some(process) = pending_process {
        process.shutdown().await;
    }
    if let Some(runtime) = runtime {
        report(
            terminal(
                &runtime.store,
                &runtime.thread_id,
                runtime.active_run.as_deref(),
                if *stopped.borrow() || user_cancelled {
                    "interrupted"
                } else {
                    "failed"
                },
                None,
            ),
            "terminal cleanup",
        );
        report(
            commit(&runtime.store, &runtime.thread_id, |projection| {
                let mut session =
                    find(projection, "providerSessions", &json!(runtime.session_id))?.clone();
                session["status"] = json!(if *stopped.borrow() || user_cancelled {
                    "stopped"
                } else {
                    "error"
                });
                session["updatedAt"] = json!(at(Utc::now()));
                if !*stopped.borrow() && !user_cancelled {
                    session["lastError"] = json!("Provider process or event continuity ended.");
                }
                Ok(vec![("provider-session.updated", session)])
            }),
            "session cleanup",
        );
        runtime.connection.process.shutdown().await;
    }
}
pub(crate) fn projection(store: &Store, id: &str) -> Result<Value, StoreError> {
    store
        .projection("thread", id)?
        .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))
}
fn report(result: Result<(), StoreError>, operation: &str) {
    if let Err(error) = result {
        tracing::error!(operation,%error,"Native provider state could not be persisted");
    }
}
pub(crate) fn find<'a>(
    projection: &'a Value,
    field: &str,
    id: &Value,
) -> Result<&'a Value, StoreError> {
    projection[field]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == *id)
        .ok_or_else(|| StoreError::InvalidCommand(format!("Missing {field} row {id}")))
}
pub(crate) fn commit(
    store: &Store,
    thread_id: &str,
    plan: impl FnOnce(&Value) -> Result<Vec<(&'static str, Value)>, StoreError>,
) -> Result<(), StoreError> {
    let command_id = format!("provider-event:{}", uuid::Uuid::new_v4());
    let now = Utc::now();
    store.dispatch(
        &command_id,
        "thread",
        thread_id,
        "provider.event",
        now,
        |transaction| {
            let projection = read_projection(transaction, "thread", thread_id)?
                .ok_or_else(|| StoreError::InvalidCommand("Thread not found.".into()))?;
            let events = plan(&projection)?
                .into_iter()
                .map(|(kind, payload)| {
                    let payload = t3_contracts::normalize_event_payload(kind, payload)
                        .map_err(|error| StoreError::InvalidCommand(error.to_string()))?;
                    Ok(event(thread_id, &command_id, kind, payload, now))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            Ok(Decision::Accepted {
                events,
                effects: vec![],
            })
        },
        thread::reduce,
    )?;
    Ok(())
}
fn native_ref(id: &str) -> Value {
    json!({"driver":"codex","nativeId":id,"strength":"strong"})
}
fn timestamp(seconds: &Value) -> Value {
    seconds
        .as_i64()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|now| json!(at(now)))
        .unwrap_or_else(|| json!(at(Utc::now())))
}
impl Actor {
    async fn connect(
        store: Store,
        providers: &ProviderRegistry,
        thread_id: &str,
        pending_process: &mut Option<crate::provider_process::ProviderProcess>,
        mcp: Option<&crate::provider_mcp::CredentialLease>,
    ) -> Result<Self, String> {
        let projection = projection(&store, thread_id).map_err(|error| error.to_string())?;
        let instance_id = projection["thread"]["modelSelection"]["instanceId"]
            .as_str()
            .unwrap();
        let instance: CodexInstance = providers
            .codex(instance_id)
            .map_err(|error| error.to_string())?;
        let project = store
            .projection(
                "project",
                projection["thread"]["projectId"].as_str().unwrap(),
            )
            .map_err(|error| error.to_string())?
            .ok_or("Project not found.")?;
        let cwd = projection["thread"]["worktreePath"]
            .as_str()
            .or_else(|| project["workspaceRoot"].as_str())
            .ok_or("Workspace root missing.")?;
        let cwd = std::fs::canonicalize(cwd).map_err(|error| error.to_string())?;
        if !cwd.is_dir() {
            return Err("Workspace root is not a directory.".into());
        }
        let mut process = instance
            .process_options(&cwd)
            .map_err(|error| error.to_string())?;
        if let Some(mcp) = mcp {
            mcp.apply_device_environment(&mut process.environment);
        }
        let process = crate::provider_process::ProviderProcess::spawn(process)
            .map_err(|error| error.to_string())?;
        *pending_process = Some(process.clone());
        let connection = CodexInstance::initialize_process(process)
            .await
            .map_err(|error| error.to_string())?;
        let session_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let capabilities: Value =
            serde_json::from_str(include_str!("codex-capabilities.json")).unwrap();
        commit(&store,thread_id,|_|Ok(vec![("provider-session.attached",json!({"id":session_id,"driver":"codex","providerInstanceId":instance_id,"status":"ready","cwd":cwd,"model":projection["thread"]["modelSelection"]["model"],"capabilities":capabilities,"createdAt":now,"updatedAt":now,"lastError":null}))])).map_err(|error|error.to_string())?;
        Ok(Self {
            store,
            thread_id: thread_id.into(),
            session_id,
            connection,
            instance_id: instance_id.into(),
            active_run: None,
            native_thread_id: None,
            native_turn_id: None,
            callbacks: HashMap::new(),
            mcp: mcp.map(|lease| lease.config.clone()),
        })
    }
    async fn effect(&mut self, effect: &Effect) -> Result<(), String> {
        match effect.request["type"].as_str() {
            Some("provider-turn.start") => {
                self.start_turn(effect.request["runId"].as_str().unwrap())
                    .await
            }
            Some("provider-turn.interrupt") => {
                if self.active_run.as_deref() != effect.request["runId"].as_str() {
                    return Err("Target run is no longer active.".into());
                }
                self.connection
                    .process
                    .request(
                        "turn/interrupt",
                        json!({"threadId":self.native_thread_id,"turnId":self.native_turn_id}),
                        Duration::from_secs(10),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            Some("runtime-request.respond") => {
                if effect.request["providerSessionId"] != self.session_id {
                    return Err("Runtime callback belongs to a different provider session.".into());
                }
                let projection =
                    projection(&self.store, &self.thread_id).map_err(|error| error.to_string())?;
                find(&projection, "runtimeRequests", &effect.request["requestId"])
                    .map_err(|error| error.to_string())?;
                let native_id = self
                    .callbacks
                    .remove(effect.request["requestId"].as_str().unwrap())
                    .ok_or("Live runtime callback no longer exists.")?;
                let result = if let Some(answers) = effect.request.get("answers") {
                    let item = projection["turnItems"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|item| item["requestId"] == effect.request["requestId"])
                        .ok_or("Runtime questions not found.")?;
                    json!({"answers":normalize_answers(answers,&item["questions"])})
                } else {
                    json!({"decision":if effect.request["decision"]=="acceptAlways"{json!("acceptForSession")}else{effect.request["decision"].clone()}})
                };
                self.connection
                    .process
                    .respond(native_id, Ok(result))
                    .await
                    .map_err(|error| error.to_string())
            }
            _ => Err("Unsupported native provider effect.".into()),
        }
    }
    async fn start_turn(&mut self, run_id: &str) -> Result<(), String> {
        let projection =
            projection(&self.store, &self.thread_id).map_err(|error| error.to_string())?;
        let run = find(&projection, "runs", &json!(run_id)).map_err(|error| error.to_string())?;
        if run["status"] != "starting" {
            return Err("Target run is no longer starting.".into());
        }
        if run["providerInstanceId"] != self.instance_id {
            return Err("Native provider handoff is not yet available.".into());
        }
        let provider_thread = find(&projection, "providerThreads", &run["providerThreadId"])
            .map_err(|error| error.to_string())?;
        let session = find(&projection, "providerSessions", &json!(self.session_id))
            .map_err(|error| error.to_string())?;
        let model = &run["modelSelection"];
        let cwd = &session["cwd"];
        if self.native_thread_id.is_none() {
            let saved = provider_thread["nativeThreadRef"]["nativeId"].as_str();
            let mut params = json!({"cwd":cwd,"model":model["model"],"config":{"tools.update_plan.enabled":true}});
            if let Some(mcp) = &self.mcp {
                params["config"]["mcp_servers"] = json!({"t3-code":{"url":mcp.endpoint,"http_headers":{"Authorization":mcp.authorization_header}}});
            }
            if let Some(id) = saved {
                params["threadId"] = json!(id);
            }
            let response = self
                .connection
                .process
                .request(
                    if saved.is_some() {
                        "thread/resume"
                    } else {
                        "thread/start"
                    },
                    params,
                    Duration::from_secs(30),
                )
                .await
                .map_err(|error| error.to_string())?;
            self.native_thread_id = Some(
                response["thread"]["id"]
                    .as_str()
                    .ok_or("Codex thread response is missing id.")?
                    .into(),
            );
        }
        let message = find(&projection, "messages", &run["userMessageId"])
            .map_err(|error| error.to_string())?;
        commit(&self.store, &self.thread_id, |projection| {
            let run = find(projection, "runs", &json!(run_id))?;
            if run["status"] != "starting" {
                return Err(StoreError::InvalidCommand(
                    "Run was interrupted before provider turn start.".into(),
                ));
            }
            let mut thread = find(projection, "providerThreads", &run["providerThreadId"])?.clone();
            thread["providerSessionId"] = json!(self.session_id);
            thread["nativeThreadRef"] = native_ref(self.native_thread_id.as_deref().unwrap());
            thread["status"] = json!("idle");
            thread["updatedAt"] = json!(at(Utc::now()));
            Ok(vec![("provider-thread.updated", thread)])
        })
        .map_err(|error| error.to_string())?;
        self.active_run = Some(run_id.into());
        let mut params = turn_params(
            &self.native_thread_id.clone().unwrap(),
            &projection["thread"],
            model,
            cwd,
            message["text"].as_str().unwrap(),
        )
        .map_err(|error| error.to_string())?;
        if let Some(mcp) = &self.mcp {
            let effort = model_option(model, "reasoningEffort").unwrap_or("medium");
            let plan = projection["thread"]["interactionMode"] == "plan";
            params["additionalContext"] = crate::provider_instructions::codex_context(
                model["model"].as_str().unwrap(),
                effort,
                mcp.browser_tools_available,
                mcp.capabilities
                    .contains(&crate::mcp_invocation::McpCapability::Device),
            );
            params["collaborationMode"] = json!({"mode":if plan {"plan"} else {"default"},"settings":{"model":model["model"],"reasoning_effort":effort,"developer_instructions":crate::provider_instructions::codex_mode(plan)}});
        }
        let response = self
            .connection
            .process
            .request("turn/start", params, Duration::from_secs(30))
            .await
            .map_err(|error| error.to_string())?;
        let native_turn_id = response["turn"]["id"]
            .as_str()
            .ok_or("Codex turn response is missing id.")?
            .to_string();
        let provider_turn_id = uuid::Uuid::new_v4().to_string();
        self.native_turn_id = Some(native_turn_id.clone());
        let now = at(Utc::now());
        commit(&self.store,&self.thread_id,|projection|{
            let mut run=find(projection,"runs",&json!(run_id))?.clone();
            if run["status"]!="starting"{return Err(StoreError::InvalidCommand("Run was interrupted while Codex started.".into()));}
            let mut attempt=find(projection,"attempts",&run["activeAttemptId"])?.clone();let mut node=find(projection,"nodes",&run["rootNodeId"])?.clone();let mut provider_thread=find(projection,"providerThreads",&run["providerThreadId"])?.clone();
            run["status"]=json!("running");run["startedAt"]=json!(now);attempt["status"]=json!("running");attempt["startedAt"]=json!(now);attempt["providerTurnId"]=json!(provider_turn_id);node["status"]=json!("running");node["startedAt"]=json!(now);node["providerTurnId"]=json!(provider_turn_id);
            provider_thread["providerSessionId"]=json!(self.session_id);provider_thread["nativeThreadRef"]=native_ref(self.native_thread_id.as_deref().unwrap());provider_thread["nativeConversationHeadRef"]=native_ref(&native_turn_id);provider_thread["status"]=json!("active");provider_thread["lastRunOrdinal"]=run["ordinal"].clone();provider_thread["updatedAt"]=json!(now);
            let turn=json!({"id":provider_turn_id,"providerThreadId":run["providerThreadId"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":native_ref(&native_turn_id),"ordinal":run["ordinal"],"status":"running","startedAt":now,"completedAt":null});
            Ok(vec![("provider-thread.updated",provider_thread),("provider-turn.updated",turn),("run.updated",run),("run-attempt.updated",attempt),("node.updated",node)])
        }).map_err(|error|error.to_string())?;
        self.active_run = Some(run_id.into());
        self.native_turn_id = Some(native_turn_id);
        Ok(())
    }
    fn notification(&mut self, method: &str, params: &Value) -> Result<(), StoreError> {
        if params.get("threadId").and_then(Value::as_str) != self.native_thread_id.as_deref() {
            return Ok(());
        }
        if let Some(id) = params.get("turnId").and_then(Value::as_str) {
            if Some(id) != self.native_turn_id.as_deref() {
                return Ok(());
            }
        }
        match method {
            "turn/started" => Ok(()),
            "turn/completed" => {
                if params["turn"]["id"].as_str() != self.native_turn_id.as_deref() {
                    return Ok(());
                }
                let status = match params["turn"]["status"].as_str() {
                    Some("completed") => "completed",
                    Some("interrupted") => "interrupted",
                    _ => "failed",
                };
                self.terminal(status, &params["turn"])?;
                self.active_run = None;
                self.native_turn_id = None;
                Ok(())
            }
            "item/started" | "item/completed" => {
                self.item(&params["item"], method == "item/completed")
            }
            "item/agentMessage/delta" => self.delta(params, "text"),
            "item/commandExecution/outputDelta" => self.delta(params, "output"),
            _ => Ok(()),
        }
    }
    fn context<'a>(&self, projection: &'a Value) -> Result<(&'a Value, &'a Value), StoreError> {
        let run = find(projection, "runs", &json!(self.active_run))?;
        let attempt = find(projection, "attempts", &run["activeAttemptId"])?;
        Ok((run, attempt))
    }
    fn base_item(
        &self,
        projection: &Value,
        run: &Value,
        attempt: &Value,
        id: &str,
        node_id: &str,
    ) -> Value {
        let now = at(Utc::now());
        json!({"id":id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node_id,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":null,"ordinal":projection["turnItems"].as_array().unwrap().len(),"status":"running","title":null,"startedAt":now,"completedAt":null,"updatedAt":now})
    }
    fn child_node(&self, run: &Value, attempt: &Value, id: &str, kind: &str) -> Value {
        json!({"id":id,"threadId":self.thread_id,"runId":run["id"],"parentNodeId":run["rootNodeId"],"rootNodeId":run["rootNodeId"],"kind":kind,"status":"running","countsForRun":false,"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"runtimeRequestId":null,"checkpointScopeId":null,"startedAt":at(Utc::now()),"completedAt":null})
    }
    fn item(&self, native: &Value, completed: bool) -> Result<(), StoreError> {
        let native_id = native["id"]
            .as_str()
            .ok_or_else(|| StoreError::InvalidCommand("Native item is missing id.".into()))?;
        let item_id = format!("{}:{native_id}", self.session_id);
        let node_id = format!("node:{item_id}");
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |projection| {
            let (run, attempt) = self.context(projection)?;
            let existing = projection["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["id"] == item_id);
            let mut item = existing
                .cloned()
                .unwrap_or_else(|| self.base_item(projection, run, attempt, &item_id, &node_id));
            item["nativeItemRef"] = native_ref(native_id);
            item["updatedAt"] = json!(now);
            let status = if !completed {
                "running"
            } else if native["status"] == "declined" {
                "cancelled"
            } else if native["status"] == "failed"
                || native["exitCode"].as_i64().is_some_and(|code| code != 0)
            {
                "failed"
            } else {
                "completed"
            };
            item["status"] = json!(status);
            if completed {
                item["completedAt"] = json!(now);
            }
            let kind = match native["type"].as_str() {
                Some("agentMessage") => "assistant_message",
                Some("commandExecution") => "tool_call",
                _ => return Ok(vec![]),
            };
            let mut node = projection["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|node| node["id"] == node_id)
                .cloned()
                .unwrap_or_else(|| self.child_node(run, attempt, &node_id, kind));
            node["nativeItemRef"] = native_ref(native_id);
            node["status"] = json!(status);
            if completed {
                node["completedAt"] = json!(now);
            }
            let mut events = vec![("node.updated", node)];
            if kind == "assistant_message" {
                let message_id = format!("message:{item_id}");
                item["type"] = json!("assistant_message");
                item["messageId"] = json!(message_id);
                item["text"] = native.get("text").cloned().unwrap_or(json!(""));
                item["streaming"] = json!(!completed);
                let message = json!({"id":message_id,"threadId":self.thread_id,"runId":run["id"],"nodeId":node_id,"role":"assistant","createdBy":"agent","creationSource":"provider","text":item["text"],"attachments":[],"streaming":!completed,"createdAt":item["startedAt"],"updatedAt":now});
                events.push(("message.updated", message));
            } else {
                item["type"] = json!("command_execution");
                item["input"] = native["command"].clone();
                if let Some(output) = native["aggregatedOutput"].as_str() {
                    item["output"] = json!(output);
                }
                if let Some(code) = native["exitCode"].as_i64() {
                    item["exitCode"] = json!(code);
                }
            }
            events.push(("turn-item.updated", item));
            Ok(events)
        })
    }
    fn delta(&self, params: &Value, field: &str) -> Result<(), StoreError> {
        let id = format!(
            "{}:{}",
            self.session_id,
            params["itemId"]
                .as_str()
                .ok_or_else(|| StoreError::InvalidCommand(
                    "Native delta missing item id.".into()
                ))?
        );
        let delta = params["delta"]
            .as_str()
            .ok_or_else(|| StoreError::InvalidCommand("Native delta missing text.".into()))?;
        let now = at(Utc::now());
        commit(&self.store, &self.thread_id, |projection| {
            let mut item = find(projection, "turnItems", &json!(id))?.clone();
            let previous = item[field].as_str().unwrap_or("");
            item[field] = json!(format!("{previous}{delta}"));
            item["updatedAt"] = json!(now);
            let mut events = vec![];
            if field == "text" {
                let mut message = find(projection, "messages", &item["messageId"])?.clone();
                message["text"] = item["text"].clone();
                message["updatedAt"] = json!(now);
                events.push(("message.updated", message));
            }
            events.push(("turn-item.updated", item));
            Ok(events)
        })
    }
    fn request(
        &mut self,
        native_id: Value,
        method: &str,
        params: &Value,
    ) -> Result<(), StoreError> {
        if params["threadId"].as_str() != self.native_thread_id.as_deref()
            || params["turnId"].as_str() != self.native_turn_id.as_deref()
        {
            return Err(StoreError::InvalidCommand(
                "Runtime request belongs to an unknown turn.".into(),
            ));
        }
        let kind = match method {
            "item/commandExecution/requestApproval" => "command",
            "item/tool/requestUserInput" => "user_input",
            _ => {
                return Err(StoreError::InvalidCommand(format!(
                    "Native callback {method} is not yet available."
                )));
            }
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let node_id = uuid::Uuid::new_v4().to_string();
        let item_id = uuid::Uuid::new_v4().to_string();
        let now = at(Utc::now());
        let native_value = native_id.clone();
        let native_id = if let Some(id) = native_id.as_str() {
            id.to_string()
        } else {
            native_id.to_string()
        };
        commit(&self.store, &self.thread_id, |projection| {
            let (run, attempt) = self.context(projection)?;
            let mut node = self.child_node(
                run,
                attempt,
                &node_id,
                if kind == "user_input" {
                    "user_input_request"
                } else {
                    "approval_request"
                },
            );
            node["status"] = json!("waiting");
            node["runtimeRequestId"] = json!(request_id);
            let request = json!({"id":request_id,"nodeId":node_id,"providerTurnId":attempt["providerTurnId"],"nativeRequestRef":native_ref(&native_id),"kind":kind,"status":"pending","responseCapability":{"type":"live","providerSessionId":self.session_id},"createdAt":now,"resolvedAt":null});
            let mut item = self.base_item(projection, run, attempt, &item_id, &node_id);
            item["status"] = json!("waiting");
            item["requestId"] = json!(request_id);
            if kind == "user_input" {
                item["type"] = json!("user_input_request");
                item["questions"] = normalize_questions(&params["questions"]);
            } else {
                item["type"] = json!("approval_request");
                item["requestKind"] = json!(kind);
                item["prompt"] = params
                    .get("reason")
                    .cloned()
                    .unwrap_or_else(|| params["command"].clone());
                item["options"] = json!([{"decision":"accept","label":"Accept"},{"decision":"decline","label":"Decline"},{"decision":"cancel","label":"Cancel"}]);
            }
            Ok(vec![
                ("node.updated", node),
                ("runtime-request.updated", request),
                ("turn-item.updated", item),
            ])
        })?;
        self.callbacks.insert(request_id, native_value);
        Ok(())
    }
    fn terminal(&self, status: &str, native: &Value) -> Result<(), StoreError> {
        terminal(
            &self.store,
            &self.thread_id,
            self.active_run.as_deref(),
            status,
            Some(native),
        )
    }
}
pub(crate) fn terminal(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    status: &str,
    native: Option<&Value>,
) -> Result<(), StoreError> {
    let Some(run_id) = run_id else { return Ok(()) };
    let completed_at = native
        .map(|native| timestamp(&native["completedAt"]))
        .unwrap_or_else(|| json!(at(Utc::now())));
    commit(store, thread_id, |projection| {
        let mut run = find(projection, "runs", &json!(run_id))?.clone();
        if !matches!(
            run["status"].as_str(),
            Some("preparing" | "queued" | "starting" | "running" | "waiting")
        ) {
            return Ok(vec![]);
        }
        run["status"] = json!(status);
        run["completedAt"] = completed_at.clone();
        let mut events = vec![];
        let mut attempt = find(projection, "attempts", &run["activeAttemptId"])?.clone();
        attempt["status"] = json!(status);
        attempt["completedAt"] = completed_at.clone();
        if !attempt["providerTurnId"].is_null() {
            let mut turn = find(projection, "providerTurns", &attempt["providerTurnId"])?.clone();
            turn["status"] = json!(status);
            turn["completedAt"] = completed_at.clone();
            events.push(("provider-turn.updated", turn));
        }
        let mut node = find(projection, "nodes", &run["rootNodeId"])?.clone();
        node["status"] = json!(status);
        node["completedAt"] = completed_at.clone();
        let mut provider_thread =
            find(projection, "providerThreads", &run["providerThreadId"])?.clone();
        provider_thread["status"] = json!(if status == "failed" { "error" } else { "idle" });
        provider_thread["updatedAt"] = completed_at.clone();
        if status == "interrupted"
            && projection["turnItems"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["runId"] == run["id"] && item["type"] == "run_interrupt_request")
        {
            let item = json!({"id":format!("{run_id}:interrupt-result"),"threadId":thread_id,"runId":run_id,"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":format!("{run_id}:interrupt-request"),"ordinal":run["ordinal"].as_u64().unwrap()*100+98,"status":"interrupted","title":"Interrupted","startedAt":completed_at,"completedAt":completed_at,"updatedAt":completed_at,"type":"run_interrupt_result","message":"Run interrupted by user"});
            events.push(("turn-item.updated", item));
        }
        let child_status = if status == "completed" {
            "cancelled"
        } else {
            status
        };
        for child in projection["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| {
                node["runId"] == run["id"]
                    && node["id"] != run["rootNodeId"]
                    && matches!(
                        node["status"].as_str(),
                        Some("running" | "waiting" | "pending")
                    )
            })
        {
            let mut child = child.clone();
            child["status"] = json!(child_status);
            child["completedAt"] = completed_at.clone();
            events.push(("node.updated", child));
        }
        for request in projection["runtimeRequests"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|request| {
                request["providerTurnId"] == attempt["providerTurnId"]
                    && request["status"] == "pending"
            })
        {
            let mut request = request.clone();
            request["status"] = json!(if status == "interrupted" {
                "cancelled"
            } else {
                "expired"
            });
            request["resolvedAt"] = completed_at.clone();
            request["responseCapability"] = json!({"type":"not_resumable","reason":if status=="interrupted"{"The run was interrupted."}else{"The provider turn has ended."}});
            events.push(("runtime-request.updated", request));
        }
        for message in projection["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["runId"] == run["id"] && message["streaming"] == true)
        {
            let mut message = message.clone();
            message["streaming"] = json!(false);
            message["updatedAt"] = completed_at.clone();
            events.push(("message.updated", message));
        }
        for item in projection["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| {
                item["runId"] == run["id"]
                    && matches!(
                        item["status"].as_str(),
                        Some("running" | "waiting" | "pending")
                    )
            })
        {
            let mut item = item.clone();
            item["status"] = json!(child_status);
            item["completedAt"] = completed_at.clone();
            item["updatedAt"] = completed_at.clone();
            if item.get("streaming").is_some() {
                item["streaming"] = json!(false);
            }
            events.push(("turn-item.updated", item));
        }
        events.extend([
            ("run.updated", run),
            ("run-attempt.updated", attempt),
            ("node.updated", node),
            ("provider-thread.updated", provider_thread),
        ]);
        Ok(events)
    })
}
pub(crate) fn fail_run(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    error: &str,
) -> Result<(), StoreError> {
    tracing::error!(thread_id,%error,"Native provider execution failed");
    terminal(store, thread_id, run_id, "failed", None)?;
    let Some(run_id) = run_id else { return Ok(()) };
    commit(store, thread_id, |projection| {
        let run = find(projection, "runs", &json!(run_id))?;
        if run["status"] != "failed" {
            return Ok(vec![]);
        }
        let attempt = find(projection, "attempts", &run["activeAttemptId"])?;
        let now = at(Utc::now());
        let message = error.chars().take(4096).collect::<String>();
        let message = if message.trim().is_empty() {
            "Provider execution failed.".into()
        } else {
            message
        };
        let item = json!({"id":format!("{run_id}:error"),"threadId":thread_id,"runId":run_id,"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":null,"ordinal":projection["turnItems"].as_array().unwrap().len(),"status":"failed","title":"Provider error","startedAt":now,"completedAt":now,"updatedAt":now,"type":"error","failure":{"class":"transport_error","message":message,"code":null,"retryable":false}});
        Ok(vec![("turn-item.updated", item)])
    })
}
pub fn turn_params(
    native_thread_id: &str,
    thread: &Value,
    model: &Value,
    cwd: &Value,
    text: &str,
) -> Result<Value, StoreError> {
    let (approval, reviewer, sandbox) = match thread["runtimeMode"].as_str() {
        Some("approval-required") => ("untrusted", "user", "readOnly"),
        Some("auto-accept-edits") => ("on-request", "user", "workspaceWrite"),
        Some("auto") => ("on-request", "auto_review", "workspaceWrite"),
        Some("full-access") => ("never", "user", "dangerFullAccess"),
        _ => return Err(StoreError::InvalidCommand("Unknown runtime mode.".into())),
    };
    let mut params = json!({"threadId":native_thread_id,"input":[{"type":"text","text":text,"text_elements":[]}],"cwd":cwd,"model":model["model"],"summary":"detailed","approvalPolicy":approval,"approvalsReviewer":reviewer,"sandboxPolicy":{"type":sandbox}});
    if let Some(effort) = model_option(model, "reasoningEffort") {
        params["effort"] = json!(effort);
    }
    if let Some(tier) = model_option(model, "serviceTier") {
        params["serviceTier"] = json!(tier);
    } else if model["options"].as_array().is_some_and(|options| {
        options
            .iter()
            .any(|option| option["id"] == "fastMode" && option["value"] == true)
    }) {
        params["serviceTier"] = json!("fast");
    }
    if thread["interactionMode"] == "plan" {
        params["collaborationMode"] = json!({"mode":"plan","settings":{"model":model["model"],"reasoning_effort":model_option(model,"reasoningEffort").unwrap_or("medium")}});
    }
    Ok(params)
}
fn model_option<'a>(model: &'a Value, id: &str) -> Option<&'a str> {
    model["options"]
        .as_array()?
        .iter()
        .find(|option| option["id"] == id)?["value"]
        .as_str()
}
fn non_empty(value: &Value, fallback: &str) -> String {
    value
        .as_str()
        .map(t3_contracts::trim_wire_string)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}
fn normalize_questions(questions: &Value) -> Value {
    Value::Array(questions.as_array().map(Vec::as_slice).unwrap_or(&[]).iter().enumerate().map(|(index,question)|{
    let options=question["options"].as_array().map(Vec::as_slice).unwrap_or(&[]).iter().enumerate().map(|(index,option)|json!({"label":non_empty(&option["label"],&format!("Option {}",index+1)),"description":non_empty(&option["description"],option["label"].as_str().unwrap_or(""))})).collect::<Vec<_>>();
    json!({"id":non_empty(&question["id"],&format!("question-{}",index+1)),"header":non_empty(&question["header"],"Question"),"question":non_empty(&question["question"],"Choose an answer."),"options":options})
}).collect())
}
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(values) => values
            .iter()
            .map(|value| {
                if value.is_null() {
                    String::new()
                } else {
                    js_string(value)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}
fn normalize_answers(answers: &Value, questions: &Value) -> Value {
    let mut output = serde_json::Map::new();
    for (id, value) in answers.as_object().into_iter().flatten() {
        if !questions
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .any(|question| question["id"] == *id)
        {
            continue;
        }
        let values = match value {
            Value::Array(values) => values.iter().map(js_string).collect::<Vec<_>>(),
            Value::Null => vec![],
            Value::Object(_) => vec![value.to_string()],
            _ => vec![js_string(value)],
        };
        output.insert(id.clone(), json!({"answers":values}));
    }
    Value::Object(output)
}
/// A native database has one owning server process. Its previous process-bound
/// callbacks and turns cannot be reconstituted or replayed after startup.
pub(crate) fn recover(
    store: &Store,
    lease: &crate::persistence::RuntimeLease,
) -> Result<(), StoreError> {
    store.recover_effects_at_startup(Utc::now(), lease)?;
    let projections =
        store.read(|connection| crate::persistence::read_projections(connection, "thread"))?;
    for projection in projections {
        let thread_id = projection["thread"]["id"].as_str().unwrap();
        for run in projection["runs"].as_array().unwrap() {
            let id = run["id"].as_str().unwrap();
            if matches!(
                run["status"].as_str(),
                Some("preparing" | "queued" | "starting" | "running" | "waiting")
            ) {
                terminal(store, thread_id, Some(id), "cancelled", None)?;
            }
        }
        if projection["providerSessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["status"] != "stopped" && session["status"] != "error")
            || projection["runtimeRequests"]
                .as_array()
                .unwrap()
                .iter()
                .any(|request| request["status"] == "pending")
        {
            commit(store, thread_id, |projection| {
                let now = at(Utc::now());
                let mut events = vec![];
                for session in projection["providerSessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|session| {
                        session["status"] != "stopped" && session["status"] != "error"
                    })
                {
                    let mut session = session.clone();
                    session["status"] = json!("stopped");
                    session["updatedAt"] = json!(now);
                    session["lastError"] = Value::Null;
                    events.push(("provider-session.updated", session));
                }
                for request in projection["runtimeRequests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|request| request["status"] == "pending")
                {
                    let mut request = request.clone();
                    request["status"] = json!("expired");
                    request["resolvedAt"] = json!(now);
                    request["responseCapability"] = json!({"type":"not_resumable","reason":"The provider runtime ended before the server restarted."});
                    events.push(("runtime-request.updated", request));
                }
                Ok(events)
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn failed_mcp_reservation_and_shutdown_before_first_poll_finish_without_waiting_for_work()
    {
        let store = Store::memory().unwrap();
        let settings = serde_json::from_value(
            json!({"providerInstances":{"codex":{"driver":"codex","enabled":false}}}),
        )
        .unwrap();
        let providers = ProviderRegistry::discover(&settings, std::path::Path::new("/tmp"))
            .await
            .unwrap();
        let registry = crate::mcp_sessions::McpSessionRegistry::new(
            "environment".parse().unwrap(),
            None,
            Arc::new(|| 0),
            100,
        );
        providers.set_mcp_sessions(crate::provider_mcp::ProviderMcpSessions::new(
            registry,
            std::path::PathBuf::from("/t3-server"),
        ));
        for already_stopped in [false, true] {
            let (_work, receive) = mpsc::channel(1);
            let (_cancel, canceled) = watch::channel(None);
            let lifetime = ActorLifetime::new(Some("codex".into()));
            let mut done = lifetime.done.subscribe();
            if already_stopped {
                lifetime.stop.send_replace(true);
            }
            let future = actor(
                store.clone(),
                providers.clone(),
                "missing-thread".into(),
                receive,
                lifetime.clone(),
                canceled,
            );
            tokio::pin!(future);
            if !already_stopped {
                assert!(
                    matches!(
                        futures_util::poll!(future.as_mut()),
                        std::task::Poll::Pending
                    ),
                    "failed reservation reached stop-aware work delivery"
                );
                lifetime.stop.send_replace(true);
            }
            tokio::time::timeout(Duration::from_secs(2), future)
                .await
                .unwrap();
            assert!(
                *done.borrow_and_update(),
                "actor ownership has finished despite open work mailbox"
            );
        }
    }
    #[test]
    fn source_runtime_policies_and_canonical_model_options_reach_codex() {
        for (mode, approval, reviewer, sandbox) in [
            ("approval-required", "untrusted", "user", "readOnly"),
            ("auto-accept-edits", "on-request", "user", "workspaceWrite"),
            ("auto", "on-request", "auto_review", "workspaceWrite"),
            ("full-access", "never", "user", "dangerFullAccess"),
        ] {
            let params=turn_params("native-thread",&json!({"runtimeMode":mode,"interactionMode":"plan"}),&json!({"model":"model","options":[{"id":"reasoningEffort","value":"high"},{"id":"serviceTier","value":"fast"}]}),&json!("/fixture"),"hello").unwrap();
            assert_eq!(params["approvalPolicy"], approval);
            assert_eq!(params["approvalsReviewer"], reviewer);
            assert_eq!(params["sandboxPolicy"]["type"], sandbox);
            assert_eq!(params["effort"], "high");
            assert_eq!(params["serviceTier"], "fast");
            assert_eq!(
                params["collaborationMode"]["settings"]["reasoning_effort"],
                "high"
            );
            assert_eq!(params["summary"], "detailed");
        }
        let thread = json!({"runtimeMode":"approval-required","interactionMode":"default"});
        let cwd = json!("/fixture");
        let params = turn_params(
            "native",
            &thread,
            &json!({"model":"model","options":[{"id":"fastMode","value":true}]}),
            &cwd,
            "hello",
        )
        .unwrap();
        assert_eq!(params["serviceTier"], "fast");
        assert!(params.get("collaborationMode").is_none());
        let params=turn_params("native",&thread,&json!({"model":"model","options":[{"id":"fastMode","value":true},{"id":"serviceTier","value":"flex"}]}),&cwd,"hello").unwrap();
        assert_eq!(params["serviceTier"], "flex");
    }
}
