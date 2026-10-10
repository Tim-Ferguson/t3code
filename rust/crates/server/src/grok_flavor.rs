//! Grok as a flavour of the ACP adapter: what `GrokAdapterV2` adds to `AcpAdapterV2`. The adapter asks the
//! flavour for the hooks that differ (the process, the `initialize` request, extension handlers, prompt
//! completion, tool normalisation, permission handling) and answers its extension events.
use crate::{
    acp_runtime::{ExtensionContext, SessionEvent, StartHooks},
    grok_acp,
    grok_instance::GrokInstance,
    xai_extension as xai,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use t3_acp::{AcpError, PayloadCodec, RpcError, types::PromptRequest};
use tokio::sync::oneshot;

const COMPLETED_PROMPT_LIMIT: usize = 128;

/// What Grok's extension handlers hand the adapter: a request it must answer, or a notification to apply.
pub enum Event {
    /// `x.ai/ask_user_question`: questions for the user; the reply is Grok's answer envelope.
    Question { params: Value, response: oneshot::Sender<Result<Value, AcpError>>, written: oneshot::Receiver<Result<(), AcpError>> },
    /// `x.ai/exit_plan_mode`: the plan the agent finished; the reply abandons Grok's own approval gate.
    ExitPlan { params: Value, response: oneshot::Sender<Result<Value, AcpError>> },
    /// A completion notification settled the active prompt. It rides the same ordered queue as the session
    /// updates, so the turn ends after every update that preceded the completion and before any that followed.
    PromptSettled,
}
struct Pending {
    session_id: String,
    prompt_id: String,
    settle: oneshot::Sender<Result<t3_acp::types::PromptResponse, AcpError>>,
}
#[derive(Default)]
struct Race {
    next: u64,
    pending: Vec<Pending>,
    completed: VecDeque<String>,
}
/// `makeXAiPromptCompletionRuntime`: Grok settles a prompt with a completion notification before (and
/// sometimes instead of) the `session/prompt` reply, so each prompt races its RPC against the matching
/// completion. Pending entries are keyed by root session and the prompt id T3 sent, so child sessions and
/// `task-completed-*` turns never settle the root turn.
#[derive(Default)]
pub struct PromptRace {
    inner: Mutex<Race>,
}
/// The stop reason a completion names, as an ACP one.
fn stop_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("cancelled") => "cancelled",
        Some("end_turn") => "end_turn",
        Some("max_tokens") => "max_tokens",
        Some("max_turn_requests") => "max_turn_requests",
        Some("refusal") => "refusal",
        _ => "end_turn",
    }
}
fn response_from_completion(complete: &Value) -> t3_acp::types::PromptResponse {
    let reason = complete["stopReason"].as_str();
    let mut meta = json!({"sessionId":complete["sessionId"]});
    if reason.is_none() {
        meta["xAiStopReasonMissing"] = json!(true);
    }
    if let Some(id) = complete["promptId"].as_str() {
        meta["promptId"] = json!(id);
        meta["requestId"] = json!(id);
    }
    if complete.get("agentResult").is_some() {
        meta["agentResult"] = complete["agentResult"].clone();
    }
    serde_json::from_value(json!({"stopReason":stop_reason(reason),"_meta":meta})).expect("prompt response")
}
/// Grok settles a failed prompt with `stopReason: "error"` and the provider's message in `agentResult`
/// before its `session/prompt` RPC error arrives. The completion wins the race, so it carries the failure.
fn failure_from_completion(complete: &Value) -> Option<AcpError> {
    match complete["stopReason"].as_str() {
        Some("rate_limit") => Some(AcpError::Request(RpcError { code: xai::RATE_LIMITED_CODE, message: "Grok usage limit reached. Try again later.".into(), data: None })),
        // Grok's raw result is unbounded provider text: kept only as data, read back at the presentation boundary.
        Some("error") => Some(AcpError::Request(RpcError { code: -32603, message: "Grok ended the turn with an error.".into(), data: complete["agentResult"].as_str().map(|text| json!({"t3XAiPromptFailureText":text})) })),
        _ => None,
    }
}
impl PromptRace {
    fn register(&self, session_id: &str) -> (String, oneshot::Receiver<Result<t3_acp::types::PromptResponse, AcpError>>) {
        let mut race = self.inner.lock().unwrap();
        race.next += 1;
        let prompt_id = format!("t3-xai-prompt-{}", race.next);
        let (settle, receiver) = oneshot::channel();
        race.pending.push(Pending { session_id: session_id.into(), prompt_id: prompt_id.clone(), settle });
        (prompt_id, receiver)
    }
    fn finish(&self, prompt_id: &str, response: Option<&t3_acp::types::PromptResponse>) {
        let mut race = self.inner.lock().unwrap();
        race.pending.retain(|entry| entry.prompt_id != prompt_id);
        let id = response
            .and_then(|response| match &response.meta {
                t3_acp::types::Optional::Value(meta) => meta.get("promptId").or_else(|| meta.get("requestId")).and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .unwrap_or_else(|| prompt_id.to_owned());
        if !id.is_empty() && !race.completed.contains(&id) {
            race.completed.push_back(id);
            while race.completed.len() > COMPLETED_PROMPT_LIMIT {
                race.completed.pop_front();
            }
        }
    }
    /// `resolveXAiPromptCompletionFallback` for a `{sessionId, promptId?, stopReason?, agentResult?}` completion.
    pub fn settle(&self, complete: &Value) -> bool {
        let mut race = self.inner.lock().unwrap();
        let prompt_id = complete["promptId"].as_str();
        if prompt_id.is_some_and(|id| race.completed.iter().any(|done| done == id)) {
            return false;
        }
        let session = complete["sessionId"].as_str().unwrap_or("");
        let index = match prompt_id {
            Some(id) => race.pending.iter().position(|entry| entry.session_id == session && entry.prompt_id == id),
            None => {
                let mine: Vec<usize> = race.pending.iter().enumerate().filter(|(_, entry)| entry.session_id == session).map(|(index, _)| index).collect();
                (mine.len() == 1).then(|| mine[0])
            }
        };
        let Some(index) = index else { return false };
        let entry = race.pending.remove(index);
        let _ = entry.settle.send(match failure_from_completion(complete) {
            Some(failure) => Err(failure),
            None => Ok(response_from_completion(complete)),
        });
        true
    }
    /// `abortPendingPromptCompletions`: a cancel settles every pending prompt of this runtime as cancelled.
    pub fn abort_all(&self) {
        let mut race = self.inner.lock().unwrap();
        for entry in race.pending.drain(..) {
            let _ = entry.settle.send(Ok(response_from_completion(&json!({"sessionId":entry.session_id,"promptId":entry.prompt_id,"stopReason":"cancelled","agentResult":null}))));
        }
    }
    /// `prompt`: the RPC raced against the completion notification, tagged with the prompt id the completion names.
    pub async fn prompt(self: Arc<Self>, client: t3_acp::Client, mut request: PromptRequest) -> Result<t3_acp::types::PromptResponse, AcpError> {
        let (prompt_id, completion) = self.register(&request.session_id);
        let mut meta = match std::mem::take(&mut request.meta) {
            t3_acp::types::Optional::Value(meta) => meta,
            _ => Default::default(),
        };
        meta.insert("promptId".into(), json!(prompt_id));
        meta.insert("requestId".into(), json!(prompt_id));
        request.meta = t3_acp::types::Optional::Value(meta);
        let result = tokio::select! {
            result = Box::pin(client.prompt_typed(request)) => result,
            settled = completion => settled.unwrap_or(Err(AcpError::Closed)),
        };
        self.finish(&prompt_id, result.as_ref().ok());
        result
    }
}
/// Everything the adapter asks of Grok: one per actor, from the thread's runtime mode.
#[derive(Clone)]
pub struct Grok {
    pub instance: GrokInstance,
    /// The mode `grok agent` launched in (`grok_acp::launch_runtime_mode`).
    pub launch_mode: String,
    pub prompts: Arc<PromptRace>,
}
fn passthrough() -> PayloadCodec<Value> {
    PayloadCodec::new(Ok, Ok)
}
impl Grok {
    pub fn new(instance: GrokInstance, runtime_mode: &str) -> Self {
        Self { instance, launch_mode: grok_acp::launch_runtime_mode(runtime_mode, false).to_owned(), prompts: Default::default() }
    }
    /// The `initialize` request, and the handlers for the `x.ai` requests and notifications Grok sends.
    pub fn hooks(&self) -> StartHooks {
        let prompts = self.prompts.clone();
        StartHooks {
            initialize: Some(crate::grok_instance::initialize_request("t3-code", false)),
            register: Some(Arc::new(move |context| -> BoxFuture<'static, ()> {
                let prompts = prompts.clone();
                Box::pin(register_extensions(context, prompts))
            })),
        }
    }
    /// Whether this permission request is one the user must answer: in Auto mode every prompt Grok sends is
    /// one its classifier blocked.
    pub fn always_asks(&self) -> bool {
        grok_acp::permission_asks(&self.launch_mode)
    }
}
async fn register_extensions(context: ExtensionContext, prompts: Arc<PromptRace>) {
    let ExtensionContext { forward_children, client, events, acknowledgements } = context;
    // Grok's subagents run in sessions of their own on this connection.
    forward_children.store(true, std::sync::atomic::Ordering::Relaxed);
    for method in xai::PROMPT_COMPLETE_METHODS {
        let (prompts, events) = (prompts.clone(), events.clone());
        client
            .handle_extension_notification(method, passthrough(), Arc::new(move |params: Value| -> BoxFuture<'static, Result<(), AcpError>> {
                let (prompts, events) = (prompts.clone(), events.clone());
                Box::pin(async move {
                    if prompts.settle(&params) {
                        let _ = events.send(SessionEvent::Grok(Event::PromptSettled));
                    }
                    Ok(())
                })
            }))
            .await;
    }
    // Turn completion rides the session notification methods that also carry `subagent_finished`.
    for method in xai::SESSION_NOTIFICATION_METHODS {
        let (prompts, events) = (prompts.clone(), events.clone());
        client
            .handle_extension_notification(method, passthrough(), Arc::new(move |params: Value| -> BoxFuture<'static, Result<(), AcpError>> {
                let (prompts, events) = (prompts.clone(), events.clone());
                Box::pin(async move {
                    if let Some(complete) = xai::prompt_complete_from_session_update(&params) {
                        if prompts.settle(&complete) {
                            let _ = events.send(SessionEvent::Grok(Event::PromptSettled));
                        }
                    }
                    Ok(())
                })
            }))
            .await;
    }
    for method in xai::ASK_USER_QUESTION_METHODS {
        let (events, acknowledgements) = (events.clone(), acknowledgements.clone());
        client.handle_extension(method, passthrough(), passthrough(), Arc::new(move |params: Value, context| -> BoxFuture<'static, Result<Value, AcpError>> {
            let (events, acknowledgements) = (events.clone(), acknowledgements.clone());
            Box::pin(async move {
                let (response, result) = oneshot::channel();
                let (ack, written) = oneshot::channel();
                acknowledgements.lock().unwrap().insert(context.request_id.clone(), ack);
                events.send(SessionEvent::Grok(Event::Question { params, response, written })).map_err(|_| AcpError::Closed)?;
                result.await.map_err(|_| AcpError::Closed)?
            })
        }));
    }
    for method in xai::EXIT_PLAN_MODE_METHODS {
        let events = events.clone();
        client.handle_extension(method, passthrough(), passthrough(), Arc::new(move |params: Value, _| -> BoxFuture<'static, Result<Value, AcpError>> {
            let events = events.clone();
            Box::pin(async move {
                let (response, result) = oneshot::channel();
                events.send(SessionEvent::Grok(Event::ExitPlan { params, response })).map_err(|_| AcpError::Closed)?;
                result.await.map_err(|_| AcpError::Closed)?
            })
        }));
    }
}
/// `promptFailure`: Grok's own failure text rides on the cause; `provider_failure::make` redacts and bounds it.
pub fn prompt_failure(cause: &AcpError) -> Value {
    let request = match cause {
        AcpError::Failure(failure) => match failure.as_ref() {
            t3_acp::errors::Failure::Request(error) => Some((error.code, error.error_message.clone(), None)),
            _ => None,
        },
        AcpError::ResponseError { error, .. } | AcpError::Request(error) => Some((error.code, error.message.clone(), error.data.as_ref().and_then(|data| data["t3XAiPromptFailureText"].as_str()).map(str::to_owned))),
        _ => None,
    };
    match request {
        Some((code, message, text)) => crate::provider_failure::make(&json!({"message":text.unwrap_or(message),"code":code.to_string(),"class":if code == xai::RATE_LIMITED_CODE{"usage_limit"}else{"provider_error"}})),
        None => crate::provider_failure::make(&json!({"class":"provider_error"})),
    }
}
