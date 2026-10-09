//! Owned Codex runtime actors. Native callbacks enter the same persisted reducer
//! as user commands; subprocess callbacks never mutate UI state directly.
use crate::{
    codex::{CodexConnection, CodexInstance},
    codex_approvals::{self, Callback},
    codex_projection::Projector,
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
    workspace: Arc<crate::workspace_prepare::WorkspaceRuntime>,
    /// Counts the effects the worker has finished, so a caller can wait for a drained outbox.
    settled: watch::Sender<u64>,
    #[cfg(test)]
    pause_actor: Arc<Mutex<Option<mpsc::UnboundedSender<(ActorLifetime, oneshot::Sender<()>)>>>>,
}
impl Drop for RuntimeOwner {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
impl RuntimeOwner {
    pub(crate) fn subscribe_settled(&self) -> watch::Receiver<u64> {
        self.settled.subscribe()
    }
    pub(crate) fn workspace(&self) -> &Arc<crate::workspace_prepare::WorkspaceRuntime> {
        &self.workspace
    }
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
    let workspace = Arc::new(crate::workspace_prepare::WorkspaceRuntime::new());
    let preparation = workspace.clone();
    let (settled, _) = watch::channel(0u64);
    let finished_effects = settled.clone();
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
                    // Checkpoint capture is git work on the workspace; it needs no provider process.
                    if effect.request["type"] == "checkpoint.capture" {
                        let (store, owner) = (store.clone(), owner.clone());
                        effects.spawn(async move {
                            let result = crate::checkpoint_capture::execute(&store, &effect).await;
                            let retry = result.as_ref().err().and_then(|error| {
                                tracing::warn!(effect = %effect.id, %error, "Checkpoint capture failed");
                                crate::checkpoint_capture::retry_delay(effect.attempt_count)
                            });
                            let outcome = result.as_ref().map(|_| ()).map_err(String::as_str);
                            if let Err(settlement) = store.finish_effect(&effect.id, &owner, Utc::now(), outcome, retry.map(|delay| Utc::now() + delay)) {
                                tracing::error!(%settlement, "Failed to settle checkpoint capture");
                            }
                        });
                        continue;
                    }
                    // A thread title is one prompt to a provider's CLI; it needs no provider session.
                    if effect.request["type"] == "thread-title.generate" {
                        let (store, owner, providers, mut stopped) = (store.clone(), owner.clone(), providers.clone(), stopped.clone());
                        effects.spawn(async move {
                            // A server that stops drops the run (and so its CLI); the effect is still pending
                            // and a later start writes the title.
                            let result = tokio::select! {
                                result = crate::text_generation_title::execute(&store, &providers, &effect) => result,
                                _ = stopped.changed() => return,
                            };
                            let outcome = result.as_ref().map(|_| ()).map_err(String::as_str);
                            if let Err(settlement) = store.finish_effect(&effect.id, &owner, Utc::now(), outcome, None) {
                                tracing::error!(%settlement, "Failed to settle thread title generation");
                            }
                        });
                        continue;
                    }
                    // Workspace preparation is git and terminal work; it needs no provider process.
                    if effect.request["type"] == "workspace.prepare" {
                        let (store, owner, providers, workspace, stopped) = (store.clone(), owner.clone(), providers.clone(), preparation.clone(), stopped.clone());
                        effects.spawn(async move {
                            let result = crate::workspace_prepare::execute(&store, &providers, &workspace, &effect, stopped).await;
                            let outcome = result.as_ref().map(|_| ()).map_err(String::as_str);
                            if let Err(settlement) = store.finish_effect(&effect.id, &owner, Utc::now(), outcome, None) {
                                tracing::error!(%settlement, "Failed to settle workspace preparation");
                            }
                        });
                        continue;
                    }
                    // A deleted thread's terminals and attachment files are cleaned without a provider.
                    if matches!(effect.request["type"].as_str(), Some("terminal.cleanup" | "attachment.cleanup")) {
                        let (store, owner, workspace) = (store.clone(), owner.clone(), preparation.clone());
                        effects.spawn(async move {
                            let result = crate::thread_cleanup::execute(&store, &workspace, &effect).await;
                            let retry = result.as_ref().err().and_then(|error| {
                                tracing::warn!(effect = %effect.id, %error, "Thread cleanup failed");
                                crate::thread_cleanup::retry_delay(effect.attempt_count)
                            });
                            let outcome = result.as_ref().map(|_| ()).map_err(String::as_str);
                            if let Err(settlement) = store.finish_effect(&effect.id, &owner, Utc::now(), outcome, retry.map(|delay| Utc::now() + delay)) {
                                tracing::error!(%settlement, "Failed to settle thread cleanup");
                            }
                        });
                        continue;
                    }
                    // Stop on a settled run ends its background work. Nothing is left running that a
                    // provider could still report on, so this needs no provider process.
                    if effect.request["type"] == "background-work.stop" {
                        let (store, owner) = (store.clone(), owner.clone());
                        // Codex keeps a settled turn's commands running in its own terminals, and Claude's
                        // background shells belong to its CLI process; a live actor of either stops those
                        // first. The other adapters hold nothing to stop.
                        let has_native_work = projection(&store, &effect.thread_id).ok().and_then(|view| view["thread"]["modelSelection"]["instanceId"].as_str().and_then(|id| providers.driver(id).ok()).map(|driver| matches!(driver, "codex" | "claudeAgent"))).unwrap_or(false);
                        let live = actors.get(&effect.thread_id).filter(|actor| !actor.is_closed() && has_native_work).cloned();
                        effects.spawn(async move {
                            if let Some(actor) = live {
                                let (complete, native) = oneshot::channel();
                                if actor.send(Work { effect: effect.clone(), complete }).await.is_ok() {
                                    if let Ok(Err(error)) = native.await {
                                        tracing::warn!(%error, "Could not stop a Codex thread's background work");
                                    }
                                }
                            }
                            let result = crate::background_work::stop(&store, &effect, &Default::default()).map_err(|error| error.to_string());
                            let outcome = result.as_ref().map(|_| ()).map_err(String::as_str);
                            if let Err(settlement) = store.finish_effect(&effect.id, &owner, Utc::now(), outcome, None) {
                                tracing::error!(%settlement, "Failed to settle background work stop");
                            }
                        });
                        continue;
                    }
                    // A rollback with no provider conversation to rewind is worktree work only.
                    if effect.request["type"] == "provider-thread.rollback"
                        && !crate::checkpoint_rollback::needs_provider(&store, &effect)
                    {
                        let (store, owner) = (store.clone(), owner.clone());
                        effects.spawn(async move {
                            let result = crate::checkpoint_rollback::run_without_provider(&store, &effect).await;
                            let retry = result.as_ref().err().and_then(|error| crate::checkpoint_rollback::settle_failure(&store, &effect, error));
                            let outcome = result.as_ref().map(|_| ()).map_err(String::as_str);
                            if let Err(settlement) = store.finish_effect(&effect.id, &owner, Utc::now(), outcome, retry) {
                                tracing::error!(%settlement, "Failed to settle checkpoint rollback");
                            }
                        });
                        continue;
                    }
                    // A replayed detach must not create a process or credential.
                    if matches!(
                        effect.request["type"].as_str(),
                        Some("provider-session.detach" | "provider-turn.steer")
                    ) && !actors
                        .get(&effect.thread_id)
                        .is_some_and(|actor| !actor.is_closed())
                    {
                        if let Err(error) =
                            store.finish_effect(&effect.id, &owner, Utc::now(), Ok(()), None)
                        {
                            tracing::error!(%error, "Failed to settle inactive provider detach");
                        }
                        continue;
                    }
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
                    if effect.request["type"]=="provider-turn.start"{crate::checkpoint_capture::capture_baseline(&store,&effect.thread_id,effect.request["runId"].as_str()).await;}
                    let result=tokio::select!{result=async{if actor.send(work).await.is_err(){Err("Provider actor stopped.".into())}else{result.await.unwrap_or_else(|_|Err("Provider actor stopped.".into()))}}=>result,_=stopped.changed()=>Err("The server stopped during provider execution.".into())};
                    if let Err(error) = &result {
                        if let Err(settlement)=fail_run(
                            &store,
                            &effect.thread_id,
                            effect.request["runId"].as_str(),
                            error,
                        ){tracing::error!(%settlement, "Failed to persist provider failure");}
                    }
                    let retry=match (&result,effect.request["type"].as_str()){(Err(error),Some("provider-thread.rollback"))=>crate::checkpoint_rollback::settle_failure(&store,&effect,error),_=>None};
                    if let Err(settlement)=store.finish_effect(
                        &effect.id,
                        &owner,
                        Utc::now(),
                        result.as_ref().map(|_| ()).map_err(String::as_str),
                        retry,
                    ){if matches!(settlement,StoreError::LeaseLost(_)){tracing::debug!(%settlement,"Provider effect was cancelled by a terminal run");}else{tracing::error!(%settlement,"Failed to settle provider effect");}}
                    });
                    continue;
                }
                Ok(None) => {}
                Err(error) => tracing::error!(%error,"Native effect claim failed"),
            }
            tokio::select! { _=stopped.changed()=>{}, _=notified.notified()=>{}, _=committed.recv()=>{}, _=scan.tick()=>{}, result=effects.join_next(),if !effects.is_empty()=>{if let Some(Err(error))=result{tracing::error!(%error,"Provider effect failed");}finished_effects.send_modify(|count|*count+=1);}, result=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Err(error))=result{tracing::error!(%error,"Provider actor failed");}} }
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
        workspace,
        settled,
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
    active_context: Option<crate::provider_turn_context::TurnContext>,
    native_thread_id: Option<String>,
    native_turn_id: Option<String>,
    callbacks: HashMap<String, Callback>,
    mcp: Option<crate::mcp_sessions::ProviderSessionConfig>,
    session: Value,
    detached: bool,
    projector: Projector,
    /// Projectors of completed turns whose commands still run, by native turn id. Codex reports a
    /// command's end whenever it happens, and the command's item is the one the turn wrote.
    retained: HashMap<String, Projector>,
    /// The native subagent threads this session has seen spawned, and what they run.
    subagents: crate::codex_subagents::Subagents,
    /// The run whose `/compact` Codex has accepted and not yet started a turn for.
    compacting: Option<String>,
    /// What the last turn start sent as `additionalContext`; Codex drops it when it compacts.
    additional_context: Option<Value>,
    /// A stop was requested for the active native turn; Codex may still report it completed.
    interrupting: bool,
    /// How long streamed text waits before it is persisted (`T3CODE_CODEX_STREAM_FLUSH_MS`).
    flush_interval: Duration,
    /// The session's MCP Apps and feedback operations, findable while the process lives.
    _live: crate::provider_live::Registration,
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
            store, providers, thread_id, work, stopped, canceled, lifetime, mcp,
        )
        .await;
    } else if instance_id.as_deref().and_then(|id| providers.driver(id).ok()) == Some("claudeAgent") {
        crate::claude_adapter::actor(store, providers, thread_id, work, stopped, canceled, lifetime, mcp).await;
    } else {
        codex_actor(store, providers, thread_id, work, stopped, canceled, mcp).await;
    }
}
async fn codex_actor(
    store: Store,
    providers: ProviderRegistry,
    thread_id: String,
    mut work: mpsc::Receiver<Work>,
    mut stopped: watch::Receiver<bool>,
    mut canceled: watch::Receiver<Option<String>>,
    mut mcp: Option<crate::provider_mcp::CredentialLease>,
) {
    let mut runtime: Option<Actor> = None;
    let mut pending_process = None;
    let mut user_cancelled = false;
    // When streamed text is next persisted; set while a notification left something buffered.
    let mut flush_due: Option<tokio::time::Instant> = None;
    loop {
        if *stopped.borrow() {
            break;
        }
        tokio::select! {
            _=stopped.changed()=>{break;},
            incoming=work.recv()=>{
                let Some(incoming)=incoming else{break};
                let starting=matches!(incoming.effect.request["type"].as_str(),Some("provider-turn.start"|"provider-turn.restart"));
                let run_id=incoming.effect.request["runId"].as_str();
                let result=tokio::select! {result=async {
                    if starting {
                        if let Some(actor) = runtime.as_mut().filter(|actor| actor.detached) {
                            // Only actual reattachment prepares a new capability
                            // grant; ordinary turns keep the current MCP client.
                            let cwd = actor.reattach_workspace()?;
                            let next = providers.reserve_mcp(&store, &thread_id, &actor.instance_id)?;
                            actor.reattach(cwd, next.as_ref())?;
                            mcp = next;
                        }
                        if let Some(mcp)=mcp.as_ref() {mcp.touch();}
                    }
                    if runtime.is_none(){runtime=Some(Actor::connect(store.clone(),&providers,&thread_id,&mut pending_process,mcp.as_ref()).await?);pending_process.take();}
                    runtime.as_mut().unwrap().effect(&incoming.effect).await
                }=>result,_=stopped.changed()=>{break;},_=async{loop{if canceled.borrow_and_update().as_deref()==run_id{break;}if canceled.changed().await.is_err(){std::future::pending::<()>().await;}}},if starting=>{user_cancelled=true;Err("The run was interrupted during provider startup.".into())}};
                let close=starting && result.is_err();
                if user_cancelled||close{work.close();}
                let _=incoming.complete.send(result);
                if user_cancelled||close{break;}
            },
            _=async{match flush_due{Some(due)=>tokio::time::sleep_until(due).await,None=>std::future::pending().await}},if flush_due.is_some()=>{
                flush_due=None;
                if let Some(runtime)=runtime.as_mut(){report(runtime.flush(),"stream flush");}
            },
            incoming=async{runtime.as_mut().unwrap().connection.events.recv().await},if runtime.is_some()=>{
                let runtime=runtime.as_mut().unwrap();
                match incoming {
                    Ok(ProcessEvent::IngressBarrier{acknowledgement})=>{acknowledgement.acknowledge();},
                    Ok(ProcessEvent::Notification{method,params})=>{if let Err(error)=runtime.notification(&method,&params){work.close();report(fail_run_captured(&runtime.store,&runtime.thread_id,runtime.active_run.as_deref(),runtime.active_context.as_ref(),&crate::provider_failure::make(&json!({"message":error.to_string(),"class":"transport_error","retryable":false}))),"provider failure");break;}},
                    Ok(ProcessEvent::Request{id,method,params})=>{match runtime.request(id.clone(),&method,&params){Ok(None)=>{},Ok(Some(answer))=>{let _=runtime.connection.process.respond(id,Ok(answer)).await;},Err(error)=>{let _=runtime.connection.process.respond(id,Err(ProcessError::Protocol(error.to_string()))).await;}}},
                    Ok(ProcessEvent::Closed(error))=>{work.close();report(fail_run_captured(&runtime.store,&runtime.thread_id,runtime.active_run.as_deref(),runtime.active_context.as_ref(),&crate::provider_failure::make(&json!({"message":error.to_string(),"class":"transport_error","retryable":false}))),"provider failure");break;},
                    Err(error)=>{work.close();report(fail_run_captured(&runtime.store,&runtime.thread_id,runtime.active_run.as_deref(),runtime.active_context.as_ref(),&crate::provider_failure::make(&json!({"cause":{"_tag":"ProviderAdapterEventStreamError"},"class":"transport_error","retryable":false}))),"provider failure");break;},
                }
                flush_due=runtime.has_pending().then(||flush_due.unwrap_or_else(||tokio::time::Instant::now()+runtime.flush_interval));
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
            terminal_captured(
                &runtime.store,
                &runtime.thread_id,
                runtime.active_run.as_deref(),
                if *stopped.borrow() || user_cancelled {
                    "interrupted"
                } else {
                    "failed"
                },
                None,
                runtime.active_context.as_ref(),
            ),
            "terminal cleanup",
        );
        report(
            commit(&runtime.store, &runtime.thread_id, |projection| {
                // A workspace detach already removed this thread's binding.
                // Cleanup still owns/reaps the process, without restoring it.
                let Some(session) = projection["providerSessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|session| session["id"] == runtime.session_id)
                else {
                    return Ok(vec![]);
                };
                let mut session = session.clone();
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
/// How long streamed text waits to be persisted. Tests that need a deterministic order set
/// `T3CODE_CODEX_STREAM_FLUSH_MS` on the instance to something longer than the test.
fn stream_flush_interval(environment: &HashMap<String, String>) -> Duration {
    let millis = environment.get("T3CODE_CODEX_STREAM_FLUSH_MS").cloned().or_else(|| std::env::var("T3CODE_CODEX_STREAM_FLUSH_MS").ok()).and_then(|value| value.trim().parse::<u64>().ok()).unwrap_or(50);
    Duration::from_millis(millis.max(1))
}
/// The entries of an `additionalContext` as response items: application context as developer
/// input, untrusted context as quoted user-side input.
fn compaction_context_items(context: &Value) -> Vec<Value> {
    context
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, entry)| {
            let value = entry["value"].as_str().unwrap_or("");
            if entry["kind"] == "untrusted" {
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":format!("<external_{key}>{value}</external_{key}>")}]})
            } else {
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":format!("<{key}>{value}</{key}>")}]})
            }
        })
        .collect()
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
        let session = json!({"id":session_id,"driver":"codex","providerInstanceId":instance_id,"status":"ready","cwd":cwd,"model":projection["thread"]["modelSelection"]["model"],"capabilities":capabilities,"createdAt":now,"updatedAt":now,"lastError":null});
        commit(&store, thread_id, |_| {
            Ok(vec![("provider-session.attached", session.clone())])
        })
        .map_err(|error| error.to_string())?;
        let projector = Projector::new(thread_id, &session_id);
        let flush_interval = stream_flush_interval(&instance.environment);
        let live = crate::provider_live::register(&session_id, crate::codex_live::live_session(&connection.process));
        Ok(Self {
            store,
            thread_id: thread_id.into(),
            session_id,
            connection,
            instance_id: instance_id.into(),
            active_run: None,
            active_context: None,
            native_thread_id: None,
            native_turn_id: None,
            callbacks: HashMap::new(),
            mcp: mcp.map(|lease| lease.config.clone()),
            session,
            detached: false,
            projector,
            retained: HashMap::new(),
            subagents: Default::default(),
            additional_context: None,
            compacting: None,
            interrupting: false,
            flush_interval,
            _live: live,
        })
    }
    async fn effect(&mut self, effect: &Effect) -> Result<(), String> {
        match effect.request["type"].as_str() {
            Some("provider-session.detach") => self.detach(effect).await,
            Some("provider-turn.start") => {
                self.start_turn(effect.request["runId"].as_str().unwrap())
                    .await
            }
            Some("provider-turn.restart") => self.restart(effect).await,
            Some("provider-thread.rollback") => self.rollback(effect).await,
            Some("provider-turn.steer") => self.steer(effect).await,
            Some("provider-turn.interrupt") => {
                if self.active_run.as_deref() != effect.request["runId"].as_str() {
                    return Err("Target run is no longer active.".into());
                }
                self.interrupt_native().await

            }
            // Stop on a settled run: the commands its turns left running are stopped; the server then
            // settles what the thread shows (`background_work::stop`).
            Some("background-work.stop") => {
                self.terminate_background_terminals().await;
                self.retained.clear();
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
                let callback = self
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
                    codex_approvals::response(&callback, effect.request["decision"].as_str().unwrap_or("decline"))
                };
                self.connection
                    .process
                    .respond(callback.native_id, Ok(result))
                    .await
                    .map_err(|error| error.to_string())
            }
            _ => Err("Unsupported native provider effect.".into()),
        }
    }
    async fn ingest(&mut self, event: ProcessEvent) -> Result<(), String> {
        match event {
            ProcessEvent::IngressBarrier { acknowledgement } => acknowledgement.acknowledge(),
            ProcessEvent::Notification { method, params } => self
                .notification(&method, &params)
                .map_err(|error| error.to_string())?,
            ProcessEvent::Request { id, method, params } => {
                let answer = match self.request(id.clone(), &method, &params) {
                    Ok(None) => return Ok(()),
                    Ok(Some(answer)) => Ok(answer),
                    Err(error) => Err(ProcessError::Protocol(error.to_string())),
                };
                self.connection.process.respond(id, answer).await.map_err(|error| error.to_string())?;
            }
            ProcessEvent::Closed(error) => return Err(error.to_string()),
        }
        Ok(())
    }
    /// Stop the native turn, then stop the terminals its commands left running, which Codex keeps
    /// after an interrupt. A terminal that cannot be stopped is logged and does not fail the stop.
    async fn interrupt_native(&mut self) -> Result<(), String> {
        self.interrupting = true;
        // Stop reaches the subagents the turn started: their turns end as stopped too.
        for (thread, turn) in self.subagents.active_turns() {
            self.subagents.mark_interrupting(&turn);
            if let Err(error) = self.connection.process.request("turn/interrupt", json!({"threadId":thread,"turnId":turn}), Duration::from_secs(10)).await {
                tracing::warn!(%error, "Could not stop a Codex subagent turn");
            }
        }
        self.connection
            .process
            .request("turn/interrupt", json!({"threadId":self.native_thread_id,"turnId":self.native_turn_id}), Duration::from_secs(10))
            .await
            .map_err(|error| error.to_string())?;
        self.terminate_background_terminals().await;
        Ok(())
    }
    /// Stop the terminals of every command still running, in the current turn and in turns that
    /// completed. A terminal that cannot be stopped is logged and does not fail the stop.
    async fn terminate_background_terminals(&mut self) {
        let processes: Vec<String> = self.projector.running_processes().into_iter().chain(self.retained.values().flat_map(Projector::running_processes)).collect();
        for process_id in processes {
            let stopped = self.connection.process.request("thread/backgroundTerminals/terminate", json!({"threadId":self.native_thread_id,"processId":process_id}), Duration::from_secs(10)).await;
            match stopped {
                Ok(response) if response["terminated"] == true => {}
                // The thread is already unloaded or the app-server is gone: nothing is left to stop.
                Err(ProcessError::Remote { code: -32600, message, .. }) if message.starts_with("thread not found:") => {}
                other => tracing::warn!(process_id, ?other, "Could not stop a Codex background terminal"),
            }
        }
    }
    /// `provider-thread.rollback`: rewind the Codex conversation to the target, then restore the
    /// worktree and record the result.
    async fn rollback(&mut self, effect: &Effect) -> Result<(), String> {
        if self.active_run.is_some() {
            return Err("Cannot roll back a Codex thread while a turn is active.".into());
        }
        let prepared = crate::checkpoint_rollback::prepare(&self.store, effect)?;
        let provider_thread = if prepared.needs_provider() {
            let view = projection(&self.store, &self.thread_id).map_err(|error| error.to_string())?;
            let session = find(&view, "providerSessions", &json!(self.session_id)).map_err(|error| error.to_string())?;
            let mut params = json!({"cwd":session["cwd"],"model":view["thread"]["modelSelection"]["model"],"config":{"tools.update_plan.enabled":true}});
            if let Some(mcp) = &self.mcp {
                params["config"]["mcp_servers"] = json!({"t3-code":{"url":mcp.endpoint,"http_headers":{"Authorization":mcp.authorization_header}}});
            }
            let rewound = crate::checkpoint_rollback::codex_rewind(&self.connection.process, &prepared, params).await?;
            // A revert leaves the thread loaded; only a conversation that moved to another native
            // thread is resumed by the next turn.
            if rewound["nativeThreadRef"]["nativeId"].as_str() != self.native_thread_id.as_deref() {
                self.native_thread_id = None;
            }
            rewound
        } else {
            prepared.provider_thread.clone()
        };
        crate::checkpoint_rollback::finish(&self.store, effect, prepared, provider_thread).await
    }
    async fn restart(&mut self, effect: &Effect) -> Result<(), String> {
        let old = self.active_context.clone().filter(|context| {
            effect.request["providerSessionId"] == self.session_id
                && context.attempt_id == effect.request["interruptedAttemptId"]
                && context.provider_turn_id == effect.request["providerTurnId"]
        });
        if let Some(old) = old {
            self.interrupt_native().await?;
            // Ingestion normally runs in the actor loop. Keep it running while
            // the control effect awaits the specific old native turn, whose
            // attempt may already be superseded in the durable projection.
            tokio::time::timeout(Duration::from_secs(10), async {
                while self
                    .active_context
                    .as_ref()
                    .is_some_and(|current| current.provider_turn_id == old.provider_turn_id)
                {
                    let event = self
                        .connection
                        .events
                        .recv()
                        .await
                        .map_err(|error| error.to_string())?;
                    self.ingest(event).await?;
                }
                Ok::<(), String>(())
            })
            .await
            .map_err(|_| "Provider turn did not terminalize before restart.".to_owned())??;
        }
        // A recovered projection without a live adapter cannot emit the old
        // terminal. Source restart skips that wait and admits the new attempt.
        self.start_turn(
            effect.request["runId"]
                .as_str()
                .ok_or("Restart run is missing.")?,
        )
        .await
    }
    async fn steer(&mut self, effect: &Effect) -> Result<(), String> {
        let view = projection(&self.store, &self.thread_id).map_err(|error| error.to_string())?;
        let message = find(&view, "messages", &effect.request["messageId"])
            .map_err(|error| error.to_string())?;
        if !crate::provider_turn_context::steering_delivery_is_current(&view, message) {
            return Ok(());
        }
        if effect.request["providerSessionId"] != self.session_id {
            return Ok(());
        }
        let turn = find(&view, "providerTurns", &effect.request["providerTurnId"])
            .map_err(|error| error.to_string())?;
        if turn["status"] == "completed" {
            crate::execution::follow_up_completed_steer(&self.store, effect)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }
        if self
            .active_context
            .as_ref()
            .is_none_or(|context| context.provider_turn_id != effect.request["providerTurnId"])
            || self.native_turn_id.is_none()
        {
            return Err("Provider turn is not active and cannot be steered.".into());
        }
        // A steering message carries its attachments as a turn start does: notes in the text
        // and each native image as a data URL.
        let mut params = json!({"threadId":self.native_thread_id,"expectedTurnId":self.native_turn_id,"input":[{"type":"text","text":crate::composer_context::provider_user_text(&message),"text_elements":[]}]});
        crate::attachment_prompt::apply_codex_attachments(&self.store, &message, &mut params).await?;
        let result = self.connection.process.request("turn/steer", params, Duration::from_secs(30)).await;
        if let Err(error) = result {
            // A terminal notification may be queued behind the failed steer
            // response. Ingest it before deciding whether to redispatch.
            while let Ok(event) = self.connection.events.try_recv() {
                self.ingest(event).await?;
            }
            let view =
                projection(&self.store, &self.thread_id).map_err(|error| error.to_string())?;
            if find(&view, "providerTurns", &effect.request["providerTurnId"])
                .map_err(|error| error.to_string())?["status"]
                == "completed"
            {
                crate::execution::follow_up_completed_steer(&self.store, effect)
                    .map_err(|error| error.to_string())?;
                return Ok(());
            }
            return Err(error.to_string());
        }
        Ok(())
    }
    async fn detach(&mut self, effect: &Effect) -> Result<(), String> {
        // Ignore another session ID and retries before reattach. The outbox
        // keeps later starts behind this detach until settlement succeeds.
        if effect.request["providerSessionId"] != self.session_id || self.detached {
            return Ok(());
        }
        if self.active_run.is_some() {
            if let Err(error) = self
                .connection
                .process
                .request(
                    "turn/interrupt",
                    json!({"threadId":self.native_thread_id,"turnId":self.native_turn_id}),
                    Duration::from_secs(10),
                )
                .await
            {
                // Source detach logs interruption failure and still unloads.
                tracing::warn!(%error, "Could not interrupt detached Codex turn");
            }
            self.terminal("interrupted", &json!({}))
                .map_err(|error| error.to_string())?;
        }
        if let Some(thread_id) = &self.native_thread_id {
            // Source shared Codex detach unsubscribes the native thread but
            // retains the app-server and its credential until reattach/release.
            if let Err(error) = self
                .connection
                .process
                .request(
                    "thread/unsubscribe",
                    json!({"threadId":thread_id}),
                    Duration::from_secs(10),
                )
                .await
            {
                tracing::warn!(%error, "Could not unload detached Codex thread");
            }
        }
        self.active_run = None;
        self.active_context = None;
        self.native_turn_id = None;
        self.native_thread_id = None;
        self.callbacks.clear();
        self.subagents.clear_active();
        self.detached = true;
        Ok(())
    }
    fn reattach_workspace(&self) -> Result<std::path::PathBuf, String> {
        let view = projection(&self.store, &self.thread_id).map_err(|error| error.to_string())?;
        let project = self
            .store
            .projection("project", view["thread"]["projectId"].as_str().unwrap())
            .map_err(|error| error.to_string())?
            .ok_or("Project not found.")?;
        let cwd = view["thread"]["worktreePath"]
            .as_str()
            .or_else(|| project["workspaceRoot"].as_str())
            .ok_or("Workspace root missing.")?;
        let cwd = std::fs::canonicalize(cwd).map_err(|error| error.to_string())?;
        if !cwd.is_dir() {
            return Err("Workspace root is not a directory.".into());
        }
        Ok(cwd)
    }
    fn reattach(
        &mut self,
        cwd: std::path::PathBuf,
        mcp: Option<&crate::provider_mcp::CredentialLease>,
    ) -> Result<(), String> {
        let view = projection(&self.store, &self.thread_id).map_err(|error| error.to_string())?;
        let mut session = self.session.clone();
        session["cwd"] = json!(cwd);
        session["model"] = view["thread"]["modelSelection"]["model"].clone();
        session["updatedAt"] = json!(at(Utc::now()));
        commit(&self.store, &self.thread_id, |_| {
            Ok(vec![("provider-session.attached", session.clone())])
        })
        .map_err(|error| error.to_string())?;
        self.session = session;
        self.mcp = mcp.map(|lease| lease.config.clone());
        self.detached = false;
        Ok(())
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
                params["excludeTurns"] = json!(true);
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
        self.interrupting = false;
        self.active_context = Some(
            crate::provider_turn_context::TurnContext::capture(&projection, run_id)
                .map_err(|error| error.to_string())?,
        );
        // `/compact` is a native request, not a prompt; Codex starts the turn that compacts and
        // says so with `turn/started`, which the run adopts.
        if message["attachments"].as_array().is_none_or(Vec::is_empty) && message["text"].as_str().is_some_and(|text| text.trim().eq_ignore_ascii_case("/compact")) {
            self.connection
                .process
                .request("thread/compact/start", json!({"threadId":self.native_thread_id}), Duration::from_secs(30))
                .await
                .map_err(|error| error.to_string())?;
            self.compacting = Some(run_id.into());
            return Ok(());
        }
        let mut params = turn_params(
            &self.native_thread_id.clone().unwrap(),
            &projection["thread"],
            model,
            cwd,
            &crate::composer_context::provider_user_text(message),
        )
        .map_err(|error| error.to_string())?;
        crate::attachment_prompt::apply_codex_attachments(&self.store, message, &mut params).await?;
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
        self.additional_context = params.get("additionalContext").cloned();
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
        self.begin_native_turn(run_id, native_turn_id)
    }
    /// The native turn exists: the run, its attempt and the provider turn are running.
    fn begin_native_turn(&mut self, run_id: &str, native_turn_id: String) -> Result<(), String> {
        let provider_turn_id = uuid::Uuid::new_v4().to_string();
        self.native_turn_id = Some(native_turn_id.clone());
        let now = at(Utc::now());
        let mut captured = self.active_context.as_ref().unwrap().clone();
        commit(&self.store,&self.thread_id,|projection|{
            if !captured.is_current(projection) { return Err(StoreError::InvalidCommand("Native start belongs to a replaced attempt.".into())); }
            let (mut run,attempt)=captured.rows(projection)?;
            if run["status"]!="starting"{return Err(StoreError::InvalidCommand("Run was interrupted while Codex started.".into()));}
            let mut attempt=attempt.clone();let mut node=find(projection,"nodes",&captured.root_node_id)?.clone();let mut provider_thread=find(projection,"providerThreads",&captured.provider_thread_id)?.clone();
            run["status"]=json!("running");run["startedAt"]=json!(now);attempt["status"]=json!("running");attempt["startedAt"]=json!(now);attempt["providerTurnId"]=json!(provider_turn_id);node["status"]=json!("running");node["startedAt"]=json!(now);node["providerTurnId"]=json!(provider_turn_id);
            provider_thread["providerSessionId"]=json!(self.session_id);provider_thread["nativeThreadRef"]=native_ref(self.native_thread_id.as_deref().unwrap());provider_thread["nativeConversationHeadRef"]=native_ref(&native_turn_id);provider_thread["status"]=json!("active");provider_thread["lastRunOrdinal"]=run["ordinal"].clone();provider_thread["updatedAt"]=json!(now);
            let turn=json!({"id":provider_turn_id,"providerThreadId":run["providerThreadId"],"nodeId":node["id"],"runAttemptId":attempt["id"],"nativeTurnRef":native_ref(&native_turn_id),"ordinal":run["ordinal"],"status":"running","startedAt":now,"completedAt":null});
            Ok(vec![("provider-thread.updated",provider_thread),("provider-turn.updated",turn),("run.updated",run),("run-attempt.updated",attempt),("node.updated",node)])
        }).map_err(|error|error.to_string())?;
        self.active_run = Some(run_id.into());
        captured.provider_turn_id = json!(provider_turn_id);
        self.projector.begin(captured.clone(), &native_turn_id);
        self.active_context = Some(captured);
        self.native_turn_id = Some(native_turn_id);
        Ok(())
    }
    fn notification(&mut self, method: &str, params: &Value) -> Result<(), StoreError> {
        if method == "account/rateLimits/updated" {
            let view = projection(&self.store, &self.thread_id)?;
            let events = self.projector.rate_limits(&view, &params["rateLimits"]);
            return self.commit_events(events);
        }
        self.apply_subagent_models()?;
        if matches!(method, "thread/settings/updated" | "model/rerouted") {
            let model = if method == "model/rerouted" { &params["toModel"] } else { &params["threadSettings"]["model"] };
            if let Some(thread) = params["threadId"].as_str() {
                self.subagents.update_model(&self.store, thread, model)?;
            }
        }
        if matches!(method, "item/started" | "item/completed") && matches!(params["item"]["type"].as_str(), Some("subAgentActivity" | "collabAgentToolCall")) {
            return self.subagent_item(method, params);
        }
        if params.get("threadId").and_then(Value::as_str) != self.native_thread_id.as_deref() {
            return self.subagent_notification(method, params);
        }
        if let Some(id) = params.get("turnId").and_then(Value::as_str) {
            if Some(id) != self.native_turn_id.as_deref() {
                return self.late_notification(id, method, params);
            }
        }
        match method {
            "turn/started" => match self.compacting.take() {
                Some(run_id) if self.native_turn_id.is_none() => {
                    let native_turn = params["turn"]["id"].as_str().ok_or_else(|| StoreError::InvalidCommand("Codex turn is missing id.".into()))?;
                    self.begin_native_turn(&run_id, native_turn.to_owned()).map_err(StoreError::InvalidCommand)
                }
                other => {
                    self.compacting = other;
                    Ok(())
                }
            },
            "turn/completed" => self.turn_completed(params),
            "item/completed" if params["item"]["type"] == "contextCompaction" => {
                self.restore_additional_context();
                self.project(method, params)
            }
            "serverRequest/resolved" => self.request_resolved(params),
            "item/started" | "item/completed" | "item/agentMessage/delta" | "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" | "item/commandExecution/outputDelta" | "item/plan/delta" | "turn/plan/updated" | "thread/tokenUsage/updated" | "error" => self.project(method, params),
            _ => Ok(()),
        }
    }
    /// An item event of a turn that already completed: its commands report when they end.
    fn late_notification(&mut self, native_turn_id: &str, method: &str, params: &Value) -> Result<(), StoreError> {
        let Some(projector) = self.retained.get_mut(native_turn_id) else { return Ok(()) };
        if matches!(method, "item/started" | "item/completed" | "item/commandExecution/outputDelta") {
            commit(&self.store, &self.thread_id, |view| projector.notification(view, method, params))?;
        }
        self.release_finished_turns()
    }
    /// A completed turn is retained only while one of its commands runs.
    fn release_finished_turns(&mut self) -> Result<(), StoreError> {
        if self.retained.is_empty() {
            return Ok(());
        }
        let view = projection(&self.store, &self.thread_id)?;
        self.retained.retain(|_, projector| projector.has_running_command(&view) || projector.has_pending());
        Ok(())
    }
    /// Whether anything streamed since the last write, in this turn or a retained one.
    fn has_pending(&self) -> bool {
        self.projector.has_pending() || self.retained.values().any(Projector::has_pending) || self.subagents.has_pending()
    }
    fn project(&mut self, method: &str, params: &Value) -> Result<(), StoreError> {
        if self.active_context.is_none() || self.projector.buffer_delta(method, params) {
            return Ok(());
        }
        let projector = &mut self.projector;
        if projector.is_bookkeeping(method, params) {
            projector.notification(&Value::Null, method, params)?;
            return Ok(());
        }
        commit(&self.store, &self.thread_id, |view| projector.notification(view, method, params))
    }
    /// Codex drops the client's developer messages when it compacts a conversation but resends
    /// `additionalContext` only when it changes, so the current entries are put back.
    fn restore_additional_context(&self) {
        let (Some(context), Some(thread)) = (&self.additional_context, &self.native_thread_id) else { return };
        let params = json!({"threadId":thread,"items":compaction_context_items(context)});
        let process = self.connection.process.clone();
        tokio::spawn(async move {
            if let Err(error) = process.request("thread/inject_items", params, Duration::from_secs(10)).await {
                tracing::warn!(%error, "Failed to restore Codex additional context after compaction.");
            }
        });
    }
    fn commit_events(&self, events: Vec<(&'static str, Value)>) -> Result<(), StoreError> {
        if events.is_empty() {
            return Ok(());
        }
        commit(&self.store, &self.thread_id, |_| Ok(events))
    }
    /// The short flush timer: persist text, output and plan content that streamed since the last write.
    fn flush(&mut self) -> Result<(), StoreError> {
        self.subagents.flush(&self.store)?;
        for projector in self.retained.values_mut() {
            if projector.has_pending() {
                commit(&self.store, &self.thread_id, |view| projector.flush(view))?;
            }
        }
        if self.active_context.is_none() {
            return Ok(());
        }
        let projector = &mut self.projector;
        commit(&self.store, &self.thread_id, |view| projector.flush(view))
    }
    fn turn_completed(&mut self, params: &Value) -> Result<(), StoreError> {
        if params["turn"]["id"].as_str() != self.native_turn_id.as_deref() {
            return Ok(());
        }
        let native = match params["turn"]["status"].as_str() {
            Some("completed") => "completed",
            Some("interrupted") => "interrupted",
            _ => "failed",
        };
        // A stop Codex finished anyway still reads as a stop.
        let status = if native == "completed" && self.interrupting { "interrupted" } else { native };
        let run_id = self.active_run.clone();
        if status != "completed" {
            self.subagents.terminalized.insert(self.native_turn_id.clone().unwrap_or_default());
        }
        // Whatever still streams is persisted before the turn ends, and the turn's usage is recorded.
        let projector = &mut self.projector;
        commit(&self.store, &self.thread_id, |view| projector.finish(view, status))?;
        self.terminal(status, &params["turn"])?;
        if status == "completed" {
            let view = projection(&self.store, &self.thread_id)?;
            if self.projector.has_running_command(&view) {
                let finished = std::mem::replace(&mut self.projector, Projector::new(&self.thread_id, &self.session_id));
                self.retained.insert(finished.native_turn(), finished);
            }
        }
        if status == "failed" {
            // Classify the root failure before holding the queue.
            let retried = self.projector.retry_failure();
            let failure = crate::provider_failure::codex_terminal(&params["turn"]["error"], self.projector.latest_failure.as_ref().or(retried.as_ref()));
            let failure = self.projector.with_reset(failure);
            fail_run_captured(&self.store, &self.thread_id, run_id.as_deref(), self.active_context.as_ref(), &failure)?;
        }
        self.active_run = None;
        self.active_context = None;
        self.native_turn_id = None;
        self.interrupting = false;
        self.callbacks.clear();
        if status != "failed" {
            settle_queue(&self.store, &self.thread_id, None)?;
        }
        Ok(())
    }
    /// Codex withdrew a request nobody answered (a stop, or it decided without the user).
    fn request_resolved(&mut self, params: &Value) -> Result<(), StoreError> {
        let Some(id) = self.callbacks.iter().find(|(_, callback)| callback.native_id == params["requestId"]).map(|(id, _)| id.clone()) else { return Ok(()) };
        self.callbacks.remove(&id);
        commit(&self.store, &self.thread_id, |view| {
            let request = find(view, "runtimeRequests", &json!(id))?;
            if request["status"] != "pending" {
                return Ok(vec![]);
            }
            let now = at(Utc::now());
            let mut events = vec![];
            let mut request = request.clone();
            request["status"] = json!("cancelled");
            request["resolvedAt"] = json!(now);
            request["responseCapability"] = json!({"type":"not_resumable","reason":"Codex withdrew the request."});
            if let Some(node) = view["nodes"].as_array().unwrap().iter().find(|node| node["id"] == request["nodeId"]) {
                let mut node = node.clone();
                node["status"] = json!("cancelled");
                node["completedAt"] = json!(now);
                events.push(("node.updated", node));
            }
            if let Some(item) = view["turnItems"].as_array().unwrap().iter().find(|item| item["requestId"] == request["id"]) {
                let mut item = item.clone();
                item["status"] = json!("cancelled");
                item["completedAt"] = json!(now);
                item["updatedAt"] = json!(now);
                events.push(("turn-item.updated", item));
            }
            events.push(("runtime-request.updated", request));
            Ok(events)
        })
    }
    /// A request Codex makes of the user. `Ok(Some(answer))` answers it at once (an elicitation the
    /// user could not answer is declined); `Ok(None)` waits for the user.
    fn request(&mut self, native_id: Value, method: &str, params: &Value) -> Result<Option<Value>, StoreError> {
        let thread = codex_approvals::request_thread(params);
        let legacy = matches!(method, "execCommandApproval" | "applyPatchApproval");
        // A subagent's request is asked here, on the top-level thread, under the subagent that
        // asked; the answer goes back to the request's own native thread.
        let root_thread = self.native_thread_id.clone();
        let from_subagent = thread.filter(|thread| Some(*thread) != root_thread.as_deref()).and_then(|thread| self.subagents.request_owner(thread, root_thread.as_deref()).flatten().map(|node| (thread, node)));
        let subagent_node = from_subagent.as_ref().filter(|(thread, _)| legacy || self.subagents.active_scope(thread).is_some_and(|scope| params["turnId"].as_str().is_none_or(|turn| turn == scope.native_turn))).map(|(_, node)| node.clone());
        let turn = match (&subagent_node, from_subagent.as_ref()) {
            (Some(_), Some((thread, _))) => self.subagents.active_scope(thread).map(|scope| scope.native_turn),
            _ => self.native_turn_id.clone(),
        };
        if (thread != root_thread.as_deref() && subagent_node.is_none()) || (!legacy && params["turnId"].as_str() != turn.as_deref() && method != "mcpServer/elicitation/request") || self.active_context.is_none() {
            if method == "mcpServer/elicitation/request" {
                return Ok(Some(json!({"action":"decline"})));
            }
            return Err(StoreError::InvalidCommand("Runtime request belongs to an unknown turn.".into()));
        }
        let view = projection(&self.store, &self.thread_id)?;
        let (request_id, events) = if method == "item/tool/requestUserInput" {
            let item = params["itemId"].as_str().ok_or_else(|| StoreError::InvalidCommand("User input request is missing its item.".into()))?;
            self.projector.user_input_artifacts(&view, item, item, params["questions"].as_array().map(Vec::as_slice).unwrap_or(&[]), None, subagent_node.as_deref())?
        } else if codex_approvals::is_supported(method) {
            if method == "mcpServer/elicitation/request" && (params["turnId"].as_str() != turn.as_deref() || !codex_approvals::elicitation_is_answerable(params)) {
                // A shape that cannot express an approval, or a request outside the turn, is declined.
                return Ok(Some(json!({"action":"decline"})));
            }
            let described = codex_approvals::describe(method, params).ok_or_else(|| StoreError::InvalidCommand("Runtime request is missing its item.".into()))?;
            self.projector.request(&view, &described, subagent_node.as_deref())?
        } else {
            return Err(StoreError::InvalidCommand(format!("Native callback {method} is not yet available.")));
        };
        commit(&self.store, &self.thread_id, |_| Ok(events))?;
        self.callbacks.insert(request_id, Callback { native_id, method: method.into(), params: params.clone() });
        Ok(None)
    }
    // --- subagents ----------------------------------------------------------------------------
    fn apply_subagent_models(&mut self) -> Result<(), StoreError> {
        self.subagents.apply_models(&self.store)
    }
    /// The running top-level turn as the subagents see it.
    fn root_scope(&self) -> Result<Option<crate::codex_subagents::Scope>, StoreError> {
        let (Some(context), Some(native_thread), Some(native_turn)) = (&self.active_context, &self.native_thread_id, &self.native_turn_id) else { return Ok(None) };
        if context.provider_turn_id.is_null() {
            return Ok(None);
        }
        let view = projection(&self.store, &self.thread_id)?;
        let run = find(&view, "runs", &json!(context.run_id))?;
        let turn = find(&view, "providerTurns", &context.provider_turn_id)?;
        Ok(Some(crate::codex_subagents::Scope {
            thread_id: self.thread_id.clone(),
            run_id: json!(context.run_id),
            native_thread: native_thread.clone(),
            native_turn: native_turn.clone(),
            provider_thread: find(&view, "providerThreads", &context.provider_thread_id)?.clone(),
            provider_turn_id: context.provider_turn_id.clone(),
            provider_turn_ordinal: turn["ordinal"].as_u64().unwrap_or(1),
            item_parent_node_id: context.root_node_id.clone(),
            root_node_id: context.root_node_id.clone(),
            app_thread: view["thread"].clone(),
            model_selection: run["modelSelection"].clone(),
            subagent: None,
        }))
    }
    /// The running turn on a native thread, the top-level one or a subagent's.
    fn scope_of(&self, thread: &str, turn: &str) -> Result<Option<crate::codex_subagents::Scope>, StoreError> {
        if Some(thread) == self.native_thread_id.as_deref() {
            return Ok(if Some(turn) == self.native_turn_id.as_deref() { self.root_scope()? } else { None });
        }
        Ok(self.subagents.scope_of(thread, turn))
    }
    /// `collabAgentToolCall` and `subAgentActivity` items: a subagent spawned, its state, or its end.
    fn subagent_item(&mut self, method: &str, params: &Value) -> Result<(), StoreError> {
        let item = &params["item"];
        let completed = method == "item/completed";
        let (Some(thread), Some(turn)) = (params["threadId"].as_str(), params["turnId"].as_str()) else { return Ok(()) };
        if completed && self.subagents.terminalized.contains(turn) {
            return Ok(());
        }
        let activity = item["type"] == "subAgentActivity";
        // A subagent's lifecycle outlives the turn that started it.
        if completed && activity && item["kind"] != "started" {
            let host = crate::codex_subagents::Host { store: &self.store, session_id: &self.session_id, process: Some(&self.connection.process) };
            return self.subagents.activity(&host, None, item);
        }
        if completed && !activity && item["tool"] != "spawnAgent" {
            let host = crate::codex_subagents::Host { store: &self.store, session_id: &self.session_id, process: Some(&self.connection.process) };
            return self.subagents.update_states(&host, item);
        }
        let Some(scope) = self.scope_of(thread, turn)? else { return Ok(()) };
        if !activity && !completed {
            return Ok(());
        }
        if activity || item["tool"] == "spawnAgent" {
            if scope.subagent.is_none() {
                self.projector.mark_subagents();
            } else {
                self.subagents.mark_usage(&scope);
            }
        }
        let host = crate::codex_subagents::Host { store: &self.store, session_id: &self.session_id, process: Some(&self.connection.process) };
        if activity {
            return self.subagents.activity(&host, Some(&scope), item);
        }
        self.subagents.register_spawned(&host, &scope, item)?;
        self.subagents.update_states(&host, item)
    }
    /// Anything else about a thread that is not the top-level one: a subagent's turns and items.
    fn subagent_notification(&mut self, method: &str, params: &Value) -> Result<(), StoreError> {
        let root = self.root_scope()?;
        let host = crate::codex_subagents::Host { store: &self.store, session_id: &self.session_id, process: Some(&self.connection.process) };
        self.subagents.notification(&host, root.as_ref(), method, params)
    }
    fn terminal(&self, status: &str, native: &Value) -> Result<(), StoreError> {
        terminal_with(
            &self.store,
            &self.thread_id,
            self.active_run.as_deref(),
            status,
            Some(native),
            self.active_context.as_ref(),
            Retain::CommandsOnCompletion,
        )
    }
}
/// Codex's todo list is rewritten whole by every `turn/plan/updated` and is never completed by the
/// turn, so ending a turn leaves it as the agent last wrote it (a `todo_list` node or item).
fn is_codex_todo_list(row: &Value) -> bool {
    let kind = if row["kind"].is_null() { &row["type"] } else { &row["kind"] };
    kind == "todo_list" && row["nativeItemRef"]["driver"] == "codex"
}
pub(crate) fn terminal(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    status: &str,
    native: Option<&Value>,
) -> Result<(), StoreError> {
    terminal_captured(store, thread_id, run_id, status, native, None)
}
/// Which turn endings leave background work (commands, dynamic tools, subagents) running. A turn that
/// ends does not end what it started in the background: the work is listed as pending and Stop
/// settles it (`background_work`). Adapters choose, as in the original, where an ending turn closes
/// its own items or leaves them to report on their own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retain {
    Nothing,
    /// A failed turn reports nothing more; its background work stays pending until Stop.
    OnFailure,
    /// The adapter keeps listening for a completed turn's commands, which report when they end.
    CommandsOnCompletion,
}
impl Retain {
    fn keeps(self, status: &str, item: &Value) -> bool {
        let kind = item["type"].as_str().unwrap_or_default();
        match self {
            Retain::Nothing => false,
            // Subagents are ended with the run that owns them (`subagents::cascade_events`).
            Retain::OnFailure => status == "failed" && crate::background_work::is_background_type(kind) && kind != "subagent",
            // A subagent a completed turn left running goes on, and reports when it ends.
            Retain::CommandsOnCompletion => status == "completed" && (matches!(kind, "command_execution" | "subagent") || crate::background_work::is_persistent_dynamic_tool(item)),
        }
    }
}
pub(crate) fn terminal_captured(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    status: &str,
    native: Option<&Value>,
    captured: Option<&crate::provider_turn_context::TurnContext>,
) -> Result<(), StoreError> {
    terminal_with(store, thread_id, run_id, status, native, captured, Retain::Nothing)
}
pub(crate) fn terminal_with(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    status: &str,
    native: Option<&Value>,
    captured: Option<&crate::provider_turn_context::TurnContext>,
    retain: Retain,
) -> Result<(), StoreError> {
    let Some(run_id) = run_id else { return Ok(()) };
    let completed_at = native
        .map(|native| timestamp(&native["completedAt"]))
        .unwrap_or_else(|| json!(at(Utc::now())));
    // The child threads of the run's subagents settle with it, each in its own commit.
    let children = projection(store, thread_id)
        .ok()
        .map(|view| find(&view, "runs", &json!(run_id)).map(|run| crate::subagents::child_threads_of_run(&view, run)).unwrap_or_default())
        .unwrap_or_default();
    crate::checkpoint_capture::commit_with_effects(store, thread_id, |projection| {
        let current = captured.is_none_or(|turn| turn.is_current(projection));
        let (mut run, captured_attempt) = if let Some(turn) = captured {
            let (run, attempt) = turn.rows(projection)?;
            (run, Some(attempt))
        } else {
            (find(projection, "runs", &json!(run_id))?.clone(), None)
        };
        let previous_status = if current {
            &run["status"]
        } else {
            captured_attempt.map_or(&run["status"], |attempt| &attempt["status"])
        };
        let old_native_running = if !current {
            captured
                .filter(|turn| !turn.provider_turn_id.is_null())
                .map(|turn| {
                    find(projection, "providerTurns", &turn.provider_turn_id)
                        .map(|native| native["status"] == "running")
                })
                .transpose()?
                .unwrap_or(false)
        } else {
            false
        };
        if !old_native_running
            && !matches!(
                previous_status.as_str(),
                Some("preparing" | "queued" | "starting" | "running" | "waiting")
            )
        {
            return Ok((vec![], vec![]));
        }
        if !current
            && captured.is_some_and(|turn| !turn.provider_turn_id.is_null())
            && !old_native_running
        {
            return Ok((vec![], vec![]));
        }
        // A completed turn leaves its run waiting until the checkpoint effect captures the
        // worktree and completes it. Stopped turns settle now and capture afterwards.
        let awaits_capture = current && status == "completed";
        run["status"] = json!(if awaits_capture { "waiting" } else { status });
        run["completedAt"] = if awaits_capture { Value::Null } else { completed_at.clone() };
        let mut events = vec![];
        let mut attempt = find(projection, "attempts", &run["activeAttemptId"])?.clone();
        if current
            || matches!(
                attempt["status"].as_str(),
                Some("running" | "waiting" | "pending" | "starting")
            )
        {
            attempt["status"] = json!(status);
            attempt["completedAt"] = completed_at.clone();
        }
        if !attempt["providerTurnId"].is_null() {
            let mut turn = find(projection, "providerTurns", &attempt["providerTurnId"])?.clone();
            turn["status"] = json!(status);
            turn["completedAt"] = completed_at.clone();
            events.push(("provider-turn.updated", turn));
        }
        let mut node = find(projection, "nodes", &run["rootNodeId"])?.clone();
        if current
            || matches!(
                node["status"].as_str(),
                Some("running" | "waiting" | "pending" | "starting")
            )
        {
            node["status"] = json!(if awaits_capture { "waiting" } else { status });
            node["completedAt"] = if awaits_capture { Value::Null } else { completed_at.clone() };
        }
        let mut provider_thread =
            find(projection, "providerThreads", &run["providerThreadId"])?.clone();
        // An adapter that keeps a failed turn's work pending also reports the thread reusable, as
        // the original's adapters do; the others mark it broken.
        provider_thread["status"] = json!(if status == "failed" && retain != Retain::OnFailure { "error" } else { "idle" });
        provider_thread["updatedAt"] = completed_at.clone();
        let interrupt_request = projection["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["runId"] == run["id"]
                    && item["type"] == "run_interrupt_request"
                    && item["nodeId"] == run["rootNodeId"]
                    && captured.is_none_or(|turn| turn.owns_item(projection, item))
            });
        // A superseded attempt's native terminal must not overwrite the Stop
        // signals belonging to this same run's current attempt.
        if status == "interrupted" && current {
            if let Some(request) = interrupt_request {
                let item = json!({"id":crate::execution::run_signal_item_id(run_id,"interrupt-result"),"threadId":thread_id,"runId":run_id,"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":request["id"],"ordinal":run["ordinal"].as_u64().unwrap()*100+98,"status":"interrupted","title":"Interrupted","startedAt":completed_at,"completedAt":completed_at,"updatedAt":completed_at,"type":"run_interrupt_result","message":"Run interrupted by user"});
                events.push(("turn-item.updated", item));
            }
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
                    && captured.is_none_or(|turn| turn.owns_node(node))
                    && node["id"] != run["rootNodeId"]
                    && !is_codex_todo_list(node)
                    && !(retain == Retain::CommandsOnCompletion && status == "completed" && node["kind"] == "subagent")
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
                    && (!attempt["providerTurnId"].is_null()
                        || projection["nodes"].as_array().unwrap().iter().any(|node| {
                            node["id"] == request["nodeId"]
                                && (node["id"] == run["rootNodeId"]
                                    || node["rootNodeId"] == run["rootNodeId"])
                        }))
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
            .filter(|message| {
                message["runId"] == run["id"]
                    && message["streaming"] == true
                    && captured.is_none_or(|turn| turn.owns_item(projection, message))
            })
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
                    && captured.is_none_or(|turn| turn.owns_item(projection, item))
                    && !is_codex_todo_list(item)
                    && !retain.keeps(status, item)
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
        events.extend(crate::subagents::cascade_events(projection, &run, status, &completed_at.as_str().map(str::to_owned).unwrap_or_default()));
        events.extend([("run-attempt.updated", attempt), ("node.updated", node.clone())]);
        let mut effects = vec![];
        if current {
            // Completed and stopped turns record a checkpoint, in the thread's effect lane.
            // A run cancelled before it ever started has nothing to capture.
            if matches!(status, "completed" | "interrupted" | "cancelled") && !(status == "cancelled" && run["startedAt"].is_null()) {
                effects.extend(crate::checkpoint_capture::capture_effect(&run, &node));
            }
            events.extend([
                ("run.updated", run),
                ("provider-thread.updated", provider_thread),
            ]);
        }
        Ok((events, effects))
    })?;
    crate::subagents::cascade_children(store, &children, status, completed_at.as_str().unwrap_or_default())
}
pub(crate) fn fail_run(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    error: &str,
) -> Result<(), StoreError> {
    fail_run_captured(
        store,
        thread_id,
        run_id,
        None,
        &crate::provider_failure::make(
            &json!({"message":error,"class":"transport_error","retryable":false}),
        ),
    )
}
pub(crate) fn fail_run_captured(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    captured: Option<&crate::provider_turn_context::TurnContext>,
    failure: &Value,
) -> Result<(), StoreError> {
    fail_run_with(store, thread_id, run_id, captured, failure, Retain::Nothing)
}
pub(crate) fn fail_run_with(
    store: &Store,
    thread_id: &str,
    run_id: Option<&str>,
    captured: Option<&crate::provider_turn_context::TurnContext>,
    failure: &Value,
    retain: Retain,
) -> Result<(), StoreError> {
    tracing::error!(thread_id, class=?failure["class"], "Native provider execution failed");
    terminal_with(store, thread_id, run_id, "failed", None, captured, retain)?;
    let Some(run_id) = run_id else { return Ok(()) };
    commit(store, thread_id, |projection| {
        let (run, attempt) = if let Some(turn) = captured {
            turn.rows(projection)?
        } else {
            let run = find(projection, "runs", &json!(run_id))?.clone();
            let attempt = find(projection, "attempts", &run["activeAttemptId"])?;
            (run, attempt)
        };
        let current = captured.is_none_or(|turn| turn.is_current(projection));
        if (current && run["status"] != "failed") || (!current && attempt["status"] != "failed") {
            return Ok(vec![]);
        }
        let now = at(Utc::now());
        if projection["turnItems"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["type"] == "error"
                    && item["nodeId"] == run["rootNodeId"]
                    && item["failure"] == *failure
            })
        {
            return Ok(vec![]);
        }
        let error_id = if attempt["attemptOrdinal"] == 1 {
            format!("{run_id}:error")
        } else {
            format!("{}:error", attempt["id"].as_str().unwrap())
        };
        let item = json!({"id":error_id,"threadId":thread_id,"runId":run_id,"nodeId":run["rootNodeId"],"providerThreadId":run["providerThreadId"],"providerTurnId":attempt["providerTurnId"],"nativeItemRef":null,"parentItemId":null,"ordinal":failure_ordinal(projection,&attempt["providerTurnId"]),"status":"failed","title":if failure["class"]=="usage_limit"{"Usage limit reached"}else{"Provider error"},"startedAt":now,"completedAt":now,"updatedAt":now,"type":"error","failure":failure});
        Ok(vec![("turn-item.updated", item)])
    })?;
    // Old-attempt failure must never classify or terminalize its replacement.
    let view = projection(store, thread_id)?;
    if captured.is_none_or(|turn| turn.is_current(&view))
        && find(&view, "runs", &json!(run_id))?["status"] == "failed"
    {
        settle_queue(store, thread_id, Some(run_id))?;
    }
    Ok(())
}
/// The failure item follows the turn's own items. A turn that never started has none.
fn failure_ordinal(projection: &Value, provider_turn_id: &Value) -> u64 {
    let items = projection["turnItems"].as_array().unwrap();
    let latest = (!provider_turn_id.is_null()).then(|| items.iter().filter(|item| item["providerTurnId"] == *provider_turn_id).filter_map(|item| item["ordinal"].as_u64()).max()).flatten();
    latest.map_or(items.len() as u64, |ordinal| ordinal + 1)
}
pub(crate) fn settle_queue(
    store: &Store,
    thread_id: &str,
    failed_run_id: Option<&str>,
) -> Result<(), StoreError> {
    crate::message_queue::settle(store, thread_id, failed_run_id).map(|_| ())
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
    // Workspace preparations that a lost process was running or about to run. Recovery cancels
    // them like every other process-bound effect; they are put back below so that the next
    // claim ends them cleanly (`workspace_prepare::execute`) instead of leaving the run
    // cancelled with a worktree behind.
    let unfinished_preparations: Vec<(String, String, Option<String>)> = store.read(|connection| {
        let mut statement = connection.prepare(
            "SELECT id,thread_id,json_extract(request_json,'$.runId') FROM rust_effect_outbox WHERE status IN ('pending','running') AND json_extract(request_json,'$.type')='workspace.prepare'",
        )?;
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    })?;
    store.recover_effects_at_startup(Utc::now(), lease)?;
    let projections =
        store.read(|connection| crate::persistence::read_projections(connection, "thread"))?;
    let mut resumed_runs = std::collections::HashSet::new();
    for (effect_id, thread_id, run_id) in unfinished_preparations {
        let Some(projection) = projections.iter().find(|projection| projection["thread"]["id"] == thread_id.as_str()) else {
            continue;
        };
        let preparing = run_id.as_deref().is_none_or(|run_id| {
            projection["runs"].as_array().is_some_and(|runs| runs.iter().any(|run| run["id"] == run_id && run["status"] == "preparing"))
        });
        if !preparing || !projection["thread"]["deletedAt"].is_null() {
            continue;
        }
        let now = Utc::now().to_rfc3339();
        store.transaction(|transaction| {
            transaction.execute(
                "UPDATE rust_effect_outbox SET status='pending',available_at=?1,updated_at=?1 WHERE id=?2 AND status='cancelled'",
                rusqlite::params![now, effect_id],
            )?;
            Ok(())
        })?;
        resumed_runs.extend(run_id);
    }
    for projection in projections {
        let thread_id = projection["thread"]["id"].as_str().unwrap();
        for run in projection["runs"].as_array().unwrap() {
            let id = run["id"].as_str().unwrap();
            // A finished turn waiting for its checkpoint capture is not process-bound: the
            // replayed capture effect completes it.
            let capturing = run["status"] == "waiting"
                && store
                    .effect(&format!("effect:checkpoint.capture:{id}"))?
                    .is_some_and(|effect| effect.status == "pending");
            if !capturing
                && !resumed_runs.contains(id)
                && matches!(
                    run["status"].as_str(),
                    Some("preparing" | "starting" | "running" | "waiting")
                )
            {
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
    for projection in
        store.read(|connection| crate::persistence::read_projections(connection, "thread"))?
    {
        settle_queue(store, projection["thread"]["id"].as_str().unwrap(), None)?;
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
            json!({"providerInstances":{"codex":{"driver":"codex","enabled":false},"claudeAgent":{"driver":"claudeAgent","enabled":false}}}),
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
    fn compaction_puts_back_developer_context_as_codex_sent_it() {
        let context = json!({"t3_code_runtime":{"kind":"application","value":"runtime"},"app":{"kind":"untrusted","value":"note"}});
        assert_eq!(
            compaction_context_items(&context),
            [
                json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"<t3_code_runtime>runtime</t3_code_runtime>"}]}),
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"<external_app>note</external_app>"}]}),
            ]
        );
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
