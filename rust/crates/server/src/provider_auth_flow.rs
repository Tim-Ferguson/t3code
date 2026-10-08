//! Owner-scoped sign-in flows. Adapters own credentials and process cleanup.
use futures_util::future::BoxFuture;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use t3_contracts::*;
use tokio::sync::{mpsc, watch};
pub type AuthResult<T> = Result<T, ProviderSetupError>;
#[derive(Clone)]
pub struct Respond {
    run: Arc<dyn Fn(ProviderAuthResponse) -> BoxFuture<'static, AuthResult<()>> + Send + Sync>,
    cleanup: StopSession,
}
impl Respond {
    pub fn new(
        run: Arc<dyn Fn(ProviderAuthResponse) -> BoxFuture<'static, AuthResult<()>> + Send + Sync>,
    ) -> Self {
        Self {
            run,
            cleanup: Arc::new(|| Box::pin(async {})),
        }
    }
    pub fn with_cleanup(mut self, cleanup: StopSession) -> Self {
        self.cleanup = cleanup;
        self
    }
}
pub type Complete = Arc<dyn Fn(String) -> BoxFuture<'static, AuthResult<()>> + Send + Sync>;
pub type StopSession = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;
#[derive(Clone)]
pub struct RoutedStop(Arc<dyn Fn() -> BoxFuture<'static, AuthResult<()>> + Send + Sync>);
impl RoutedStop {
    pub fn new(stop: Arc<dyn Fn() -> BoxFuture<'static, AuthResult<()>> + Send + Sync>) -> Self {
        Self(stop)
    }
}
impl From<StopSession> for RoutedStop {
    fn from(stop: StopSession) -> Self {
        Self(Arc::new(move || {
            let stop = stop.clone();
            Box::pin(async move {
                stop().await;
                Ok(())
            })
        }))
    }
}
pub trait AuthBackend: Send + Sync + 'static {
    fn methods(&self) -> BoxFuture<'static, AuthResult<Vec<ProviderAuthMethod>>>;
    fn authenticate(
        &self,
        method: String,
        context: AuthContext,
    ) -> BoxFuture<'static, AuthResult<()>>;
    /// Await owned adapter resources after success, failure, timeout or cancellation.
    fn cleanup(&self) -> BoxFuture<'static, ()>;
    fn cleanup_methods(&self) -> BoxFuture<'static, ()>;
    fn logout(&self) -> BoxFuture<'static, AuthResult<Option<String>>>;
}
#[derive(Clone)]
pub struct AuthContext {
    inner: Weak<Inner>,
    pub flow_id: String,
    pub expires_at: i64,
    pub return_url: Option<String>,
    pub callback_mode: Option<ProviderAuthCallbackMode>,
}
impl AuthContext {
    pub fn set_interaction(
        &self,
        interaction: ProviderAuthInteraction,
        respond: Option<Respond>,
        complete: Option<Complete>,
    ) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = inner.state.lock().unwrap();
        let Some(flow) = state.active.as_mut().filter(|flow| flow.id == self.flow_id) else {
            return;
        };
        flow.respond = respond;
        flow.complete = complete;
        state.snapshot.phase = ProviderAuthPhase::Waiting;
        state.snapshot.authorization_url = match &interaction {
            ProviderAuthInteraction::Browser { url, .. }
            | ProviderAuthInteraction::DeviceCode { url, .. } => Some(url.0.to_string()),
            _ => None,
        };
        state.snapshot.interaction = Some(Some(Some(interaction)));
        state.snapshot.message = Some("Complete sign-in to continue.".into());
        state.publish();
    }
    pub fn verifying(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut state = inner.state.lock().unwrap();
        let Some(flow) = state.active.as_mut().filter(|flow| flow.id == self.flow_id) else {
            return;
        };
        flow.respond = None;
        flow.complete = None;
        state.snapshot.phase = ProviderAuthPhase::Verifying;
        state.snapshot.authorization_url = None;
        state.snapshot.interaction = Some(Some(None));
        state.snapshot.message = Some("Checking provider sign-in.".into());
        state.publish();
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Operation {
    Idle,
    Auth,
    Stopping,
    Closed,
}
#[derive(Clone)]
struct Job {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}
impl Job {
    async fn stop(&self) {
        self.cancel.send_replace(true);
        let mut done = self.done.clone();
        if !*done.borrow() {
            let _ = done.wait_for(|done| *done).await;
        }
    }
}
struct Flow {
    id: String,
    owner: String,
    expires_at: i64,
    job: Job,
    respond: Option<Respond>,
    complete: Option<Complete>,
    response: Option<Job>,
}
struct State {
    operation: Operation,
    active: Option<Flow>,
    owner: Option<String>,
    snapshot: ProviderAuthState,
    next: u64,
    subscribers: HashMap<u64, (String, mpsc::UnboundedSender<ProviderAuthState>)>,
    sessions: HashMap<u64, Weak<AccessInner>>,
}
impl State {
    fn visible(&self, owner: &str) -> ProviderAuthState {
        let mut state = self.snapshot.clone();
        if self
            .owner
            .as_deref()
            .is_some_and(|current| current != owner)
        {
            state.flow_id = None;
            state.authorization_url = None;
            state.interaction = Some(Some(None));
            state.expires_at = None;
            if self.active.is_some() {
                state.message = Some("Sign-in is in progress in another client.".into());
            }
        }
        state
    }
    fn publish(&mut self) {
        let values: Vec<_> = self
            .subscribers
            .iter()
            .map(|(&id, (owner, sender))| (id, sender.clone(), self.visible(owner)))
            .collect();
        for (id, sender, value) in values {
            if sender.send(value).is_err() {
                self.subscribers.remove(&id);
            }
        }
    }
}
struct Inner {
    state: Mutex<State>,
    backend: Arc<dyn AuthBackend>,
    instance: String,
    default_method: Option<String>,
    timeout: Duration,
    refresh_after_auth: bool,
    admission: Arc<tokio::sync::Mutex<()>>,
    startup: Mutex<Option<Job>>,
}
struct Owner(Arc<Inner>);
impl Drop for Owner {
    fn drop(&mut self) {
        let active = {
            let mut state = self.0.state.lock().unwrap();
            state.operation = Operation::Closed;
            state.active.take()
        };
        if let Some(job) = self.0.startup.lock().unwrap().take() {
            job.cancel.send_replace(true);
        }
        if let Some(flow) = active {
            flow.job.cancel.send_replace(true);
            if let Some(response) = &flow.response {
                response.cancel.send_replace(true);
            }
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Some(response) = flow.response {
                        response.stop().await;
                    }
                    flow.job.stop().await;
                });
            }
        }
    }
}
#[derive(Clone)]
pub struct AuthFlow(Arc<Owner>);
pub struct AuthSubscription {
    inner: Weak<Inner>,
    id: u64,
    receiver: mpsc::UnboundedReceiver<ProviderAuthState>,
}
impl AuthSubscription {
    pub async fn recv(&mut self) -> Option<ProviderAuthState> {
        self.receiver.recv().await
    }
}
impl Drop for AuthSubscription {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.state.lock().unwrap().subscribers.remove(&self.id);
        }
    }
}
struct AccessInner {
    inner: Weak<Inner>,
    id: u64,
    stop: StopSession,
    job: Mutex<Option<Job>>,
}
impl AccessInner {
    fn begin_stop(&self) -> Job {
        let mut owned = self.job.lock().unwrap();
        if let Some(job) = owned.as_ref() {
            return job.clone();
        }
        let (cancel, _) = watch::channel(false);
        let (done, receiver) = watch::channel(false);
        let stop = self.stop.clone();
        tokio::spawn(async move {
            stop().await;
            done.send_replace(true);
        });
        let job = Job {
            cancel,
            done: receiver,
        };
        *owned = Some(job.clone());
        job
    }
}
impl Drop for AccessInner {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.state.lock().unwrap().sessions.remove(&self.id);
        }
        if self.job.lock().unwrap().is_none() {
            let stop = self.stop.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    stop().await;
                });
            }
        }
    }
}
#[derive(Clone)]
pub struct SessionAccess(Arc<AccessInner>);
impl SessionAccess {
    pub async fn dispose(&self) {
        self.0.begin_stop().stop().await;
    }
}
fn safe_error(instance: &str, operation: &str, detail: &str) -> ProviderSetupError {
    ProviderSetupError {
        tag: ProviderSetupErrorTag::ProviderSetupError,
        instance_id: instance.parse().expect("validated instance"),
        operation: operation.into(),
        detail: detail.into(),
        cause: None,
    }
}
impl AuthFlow {
    pub fn new(
        instance: String,
        backend: Arc<dyn AuthBackend>,
        default_method: Option<String>,
        timeout: Duration,
        owner: ProviderCredentialOwner,
        refresh_after_auth: bool,
    ) -> Self {
        let snapshot = ProviderAuthState {
            instance_id: instance.parse().expect("validated instance"),
            phase: ProviderAuthPhase::Idle,
            flow_id: None,
            authorization_url: None,
            expires_at: None,
            message: None,
            methods: None,
            interaction: Some(Some(None)),
            credential_owner: Some(Some(owner)),
        };
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                operation: Operation::Idle,
                active: None,
                owner: None,
                snapshot,
                next: 0,
                subscribers: HashMap::new(),
                sessions: HashMap::new(),
            }),
            backend,
            instance,
            default_method,
            timeout,
            refresh_after_auth,
            admission: Arc::new(tokio::sync::Mutex::new(())),
            startup: Mutex::new(None),
        });
        let (cancel, mut cancellation) = watch::channel(false);
        let (done, receiver) = watch::channel(false);
        *inner.startup.lock().unwrap() = Some(Job {
            cancel,
            done: receiver,
        });
        let initialized = inner.clone();
        tokio::spawn(async move {
            let result = tokio::select! {biased;result=initialized.backend.methods()=>Some(result),_=cancellation.wait_for(|cancel|*cancel)=>None};
            initialized.backend.cleanup_methods().await;
            if let Some(result) = result {
                publish_methods(&initialized, result);
            }
            done.send_replace(true);
        });
        Self(Arc::new(Owner(inner)))
    }
    fn inner(&self) -> &Arc<Inner> {
        &self.0.0
    }
    pub fn snapshot(&self, owner: &str) -> ProviderAuthState {
        self.inner().state.lock().unwrap().visible(owner)
    }
    pub fn subscribe(&self, owner: String) -> AuthSubscription {
        let inner = self.inner();
        let (sender, receiver) = {
            let (sender, receiver) = mpsc::unbounded_channel();
            (sender, receiver)
        };
        let mut state = inner.state.lock().unwrap();
        let id = state.next;
        state.next += 1;
        let _ = sender.send(state.visible(&owner));
        state.subscribers.insert(id, (owner, sender));
        AuthSubscription {
            inner: Arc::downgrade(inner),
            id,
            receiver,
        }
    }
    pub fn is_changing_credentials(&self) -> bool {
        self.inner().state.lock().unwrap().operation != Operation::Idle
    }
    /// Retained by a successful session's parent lifetime, not just setup.
    pub async fn admit_session(&self, stop: StopSession) -> AuthResult<SessionAccess> {
        let inner = self.inner();
        let _admission = inner.admission.lock().await;
        let mut state = inner.state.lock().unwrap();
        if state.operation != Operation::Idle {
            return Err(safe_error(
                &inner.instance,
                "session",
                "Provider sign-in is changing. Try again after it finishes.",
            ));
        }
        let id = state.next;
        state.next += 1;
        let access = Arc::new(AccessInner {
            inner: Arc::downgrade(inner),
            id,
            stop,
            job: Mutex::new(None),
        });
        state.sessions.insert(id, Arc::downgrade(&access));
        Ok(SessionAccess(access))
    }
    /// Shared-credential changes close retained scopes, preserving discovered
    /// methods. The owned cleanup keeps session admission closed on caller drop.
    pub async fn invalidate(&self) {
        let inner = self.inner().clone();
        let admission = inner.admission.clone().lock_owned().await;
        if inner.state.lock().unwrap().operation != Operation::Idle {
            return;
        }
        let _ = tokio::spawn(async move {
            let _admission = admission;
            stop_sessions(&inner).await;
            let mut state = inner.state.lock().unwrap();
            if state.operation == Operation::Closed {
                return;
            }
            state.owner = None;
            state.snapshot.phase = ProviderAuthPhase::Idle;
            state.snapshot.flow_id = None;
            state.snapshot.authorization_url = None;
            state.snapshot.expires_at = None;
            state.snapshot.interaction = Some(Some(None));
            state.snapshot.message = Some("This provider's shared sign-in changed.".into());
            state.publish();
        })
        .await;
    }
    pub async fn start(
        &self,
        owner: String,
        method: Option<String>,
        return_url: Option<String>,
        callback_mode: Option<ProviderAuthCallbackMode>,
        stop_routed: impl Into<RoutedStop>,
    ) -> AuthResult<ProviderAuthState> {
        let stop_routed = stop_routed.into();
        let inner = self.inner();
        let _admission = inner.admission.lock().await;
        let mut state = inner.state.lock().unwrap();
        if state.operation == Operation::Auth
            && state
                .active
                .as_ref()
                .is_some_and(|flow| flow.owner == owner)
        {
            return Ok(state.snapshot.clone());
        }
        if state.operation != Operation::Idle {
            return Err(safe_error(
                &inner.instance,
                "start",
                "Provider setup is already in progress.",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let expires_at = chrono::Utc::now().timestamp_millis() + inner.timeout.as_millis() as i64;
        let (cancel, cancellation) = watch::channel(false);
        let (done, completed) = watch::channel(false);
        state.active = Some(Flow {
            id: id.clone(),
            owner: owner.clone(),
            expires_at,
            job: Job {
                cancel,
                done: completed,
            },
            respond: None,
            complete: None,
            response: None,
        });
        state.operation = Operation::Auth;
        state.owner = Some(owner);
        state.snapshot.phase = ProviderAuthPhase::Starting;
        state.snapshot.flow_id = Some(BoundedTrimmedString(id.parse().unwrap()));
        state.snapshot.expires_at = Some(
            chrono::DateTime::from_timestamp_millis(expires_at)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        );
        state.snapshot.interaction = Some(Some(None));
        state.snapshot.authorization_url = None;
        state.snapshot.message = Some("Starting sign-in.".into());
        state.publish();
        let snapshot = state.snapshot.clone();
        let inner = inner.clone();
        tokio::spawn(async move {
            run_authentication(
                inner,
                id,
                expires_at,
                method,
                return_url,
                callback_mode,
                stop_routed,
                cancellation,
            )
            .await;
            done.send_replace(true);
        });
        Ok(snapshot)
    }
    pub async fn cancel(&self, owner: &str, id: &str) -> AuthResult<ProviderAuthState> {
        let inner = self.inner().clone();
        let admission = inner.admission.clone().lock_owned().await;
        let flow = {
            let mut state = inner.state.lock().unwrap();
            require_flow(&inner, &state, owner, id)?;
            state.snapshot.phase = ProviderAuthPhase::Cancelled;
            state.snapshot.interaction = Some(Some(None));
            state.snapshot.authorization_url = None;
            state.snapshot.expires_at = None;
            state.snapshot.message = Some("Sign-in cancelled.".into());
            let flow = state.active.take().unwrap();
            state.operation = Operation::Stopping;
            state.publish();
            flow
        };
        drop(admission);
        // The operation is uninterruptible once admitted; idle follows cleanup.
        tokio::spawn(async move {
            stop_flow(flow).await;
            let mut state = inner.state.lock().unwrap();
            if state.operation != Operation::Closed {
                state.operation = Operation::Idle;
            }
            Ok(state.snapshot.clone())
        })
        .await
        .map_err(|_| {
            safe_error(
                &self.inner().instance,
                "cancel",
                "Could not cancel sign-in.",
            )
        })?
    }
    pub async fn respond(
        &self,
        owner: &str,
        input: ProviderAuthRespondInput,
    ) -> AuthResult<ProviderAuthState> {
        let inner = self.inner().clone();
        let flow_id = input.flow_id.0.to_string();
        let (callback, id, cancel, mut cancellation, done, completed) = {
            let mut state = inner.state.lock().unwrap();
            let flow = require_flow(&inner, &state, owner, &flow_id)?;
            let interaction = state
                .snapshot
                .interaction
                .as_ref()
                .and_then(Option::as_ref)
                .and_then(Option::as_ref);
            if interaction.is_none_or(|interaction| {
                interaction.id() != input.interaction_id.0.as_str()
                    || !same_response(interaction, &input.response)
            }) || flow.respond.is_none()
            {
                return Err(safe_error(
                    &inner.instance,
                    "respond",
                    "This sign-in interaction is no longer available.",
                ));
            }
            if flow.response.is_some() {
                return Err(safe_error(
                    &inner.instance,
                    "respond",
                    "A sign-in response is already in progress.",
                ));
            }
            let callback = flow.respond.clone().unwrap();
            let (cancel, cancellation) = watch::channel(false);
            let (done, completed) = watch::channel(false);
            let flow = state.active.as_mut().unwrap();
            flow.response = Some(Job {
                cancel: cancel.clone(),
                done: completed.clone(),
            });
            (
                callback,
                flow.id.clone(),
                cancel,
                cancellation,
                done,
                completed,
            )
        };
        struct Cancel(watch::Sender<bool>);
        impl Drop for Cancel {
            fn drop(&mut self) {
                self.0.send_replace(true);
            }
        }
        let _cancel = Cancel(cancel);
        let owner = owner.to_owned();
        let (result_sender, result) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let response = tokio::select! {biased;response=(callback.run)(input.response)=>response,_=cancellation.wait_for(|cancel|*cancel)=>Err(safe_error(&inner.instance,"respond","This sign-in interaction is no longer available."))};
            (callback.cleanup)().await;
            let snapshot = {
                let mut state = inner.state.lock().unwrap();
                if let Some(flow) = state.active.as_mut().filter(|flow| flow.id == id) {
                    flow.response = None;
                }
                state.visible(&owner)
            };
            done.send_replace(true);
            let _ = result_sender.send(response.map(|_| snapshot));
        });
        let _completed = completed;
        result.await.map_err(|_| {
            safe_error(
                &self.inner().instance,
                "respond",
                "The sign-in response stopped.",
            )
        })?
    }
    pub async fn complete(
        &self,
        owner: &str,
        input: ProviderAuthCompleteInput,
    ) -> AuthResult<ProviderAuthState> {
        let inner = self.inner();
        let _admission = inner.admission.lock().await;
        let callback = {
            let state = inner.state.lock().unwrap();
            let flow = require_flow(inner, &state, owner, input.flow_id.0.as_str())?;
            let allowed = state.snapshot.phase == ProviderAuthPhase::Waiting
                && matches!(
                    state.snapshot.interaction,
                    Some(Some(Some(ProviderAuthInteraction::Browser {
                        accepts_callback: Some(true),
                        ..
                    })))
                );
            if !allowed || flow.complete.is_none() {
                return Err(safe_error(
                    &inner.instance,
                    "complete",
                    "This sign-in does not accept a redirect URL.",
                ));
            }
            flow.complete.clone().unwrap()
        };
        callback(input.callback_url.0.to_string()).await?;
        Ok(self.snapshot(owner))
    }
    pub async fn logout(
        &self,
        stop_routed: impl Into<RoutedStop>,
    ) -> AuthResult<ProviderAuthState> {
        let stop_routed = stop_routed.into();
        let inner = self.inner().clone();
        let admission = inner.admission.clone().lock_owned().await;
        let flow = {
            let mut state = inner.state.lock().unwrap();
            if !matches!(state.operation, Operation::Idle | Operation::Auth) {
                return Err(safe_error(
                    &inner.instance,
                    "logout",
                    "Provider setup is already stopping.",
                ));
            }
            state.operation = Operation::Stopping;
            state.active.take()
        };
        drop(admission);
        tokio::spawn(async move {
            if let Some(flow) = flow {
                stop_flow(flow).await;
            }
            let stopped = (stop_routed.0)().await;
            stop_sessions(&inner).await;
            let result = match stopped {
                Ok(()) => inner.backend.logout().await,
                Err(error) => Err(error),
            };
            inner.backend.cleanup().await;
            if result.is_ok() && inner.refresh_after_auth {
                refresh_methods(&inner).await;
            }
            let mut state = inner.state.lock().unwrap();
            state.owner = None;
            state.snapshot.flow_id = None;
            state.snapshot.phase = if result.is_ok() {
                ProviderAuthPhase::Idle
            } else {
                ProviderAuthPhase::Failed
            };
            state.snapshot.interaction = Some(Some(None));
            state.snapshot.authorization_url = None;
            state.snapshot.expires_at = None;
            state.snapshot.message = Some(match &result {
                Ok(message) => message.clone().unwrap_or_else(|| "Signed out.".into()),
                Err(_) => "Could not sign out. Try again.".into(),
            });
            if state.operation != Operation::Closed {
                state.operation = Operation::Idle;
            }
            state.publish();
            result.map(|_| state.snapshot.clone())
        })
        .await
        .map_err(|_| {
            safe_error(
                &self.inner().instance,
                "logout",
                "Could not sign out. Try again.",
            )
        })?
    }
    pub async fn shutdown(&self) {
        let startup = self.inner().startup.lock().unwrap().take();
        if let Some(startup) = &startup {
            startup.cancel.send_replace(true);
        }
        let flow = {
            let mut state = self.inner().state.lock().unwrap();
            state.operation = Operation::Closed;
            state.active.take()
        };
        if let Some(flow) = flow {
            stop_flow(flow).await;
        }
        if let Some(startup) = startup {
            startup.stop().await;
        }
        // Admitted sessions belong to their parent, even after a settings rebuild.
    }
}
fn require_flow<'a>(
    inner: &Inner,
    state: &'a State,
    owner: &str,
    id: &str,
) -> AuthResult<&'a Flow> {
    state
        .active
        .as_ref()
        .filter(|flow| {
            state.operation == Operation::Auth
                && flow.owner == owner
                && flow.id == id
                && chrono::Utc::now().timestamp_millis() < flow.expires_at
        })
        .ok_or_else(|| {
            safe_error(
                &inner.instance,
                "respond",
                "This sign-in is no longer active in this client.",
            )
        })
}
fn same_response(interaction: &ProviderAuthInteraction, response: &ProviderAuthResponse) -> bool {
    matches!(
        (interaction, response),
        (
            ProviderAuthInteraction::Browser { .. },
            ProviderAuthResponse::Browser { .. }
        ) | (
            ProviderAuthInteraction::Terminal { .. },
            ProviderAuthResponse::Terminal { .. }
        ) | (
            ProviderAuthInteraction::Credentials { .. },
            ProviderAuthResponse::Credentials { .. }
        )
    )
}
async fn stop_flow(flow: Flow) {
    if let Some(response) = flow.response {
        response.stop().await;
    }
    flow.job.stop().await;
}
async fn stop_sessions(inner: &Inner) {
    let sessions: Vec<_> = inner
        .state
        .lock()
        .unwrap()
        .sessions
        .values()
        .filter_map(Weak::upgrade)
        .collect();
    futures_util::future::join_all(sessions.into_iter().map(|session| async move {
        session.begin_stop().stop().await;
    }))
    .await;
}
async fn refresh_methods(inner: &Inner) {
    let result = inner.backend.methods().await;
    publish_methods(inner, result);
}
fn publish_methods(inner: &Inner, result: AuthResult<Vec<ProviderAuthMethod>>) {
    let mut state = inner.state.lock().unwrap();
    match result {
        Ok(methods) => state.snapshot.methods = Some(ForwardCompatibleArray(methods)),
        Err(error) => {
            if state.snapshot.methods.is_none() {
                state.snapshot.methods = Some(ForwardCompatibleArray(vec![]));
            }
            state.snapshot.message = Some(error.detail);
        }
    }
    state.publish();
}
async fn run_authentication(
    inner: Arc<Inner>,
    id: String,
    expires_at: i64,
    method: Option<String>,
    return_url: Option<String>,
    callback_mode: Option<ProviderAuthCallbackMode>,
    stop_routed: RoutedStop,
    mut cancellation: watch::Receiver<bool>,
) {
    let authenticate = async {
        let methods = inner.backend.methods().await?;
        {
            let mut state = inner.state.lock().unwrap();
            if state.active.as_ref().is_some_and(|flow| flow.id == id) {
                state.snapshot.methods = Some(ForwardCompatibleArray(methods.clone()));
                state.publish();
            }
        }
        let method = method
            .or_else(|| inner.default_method.clone())
            .or_else(|| methods.first().map(|method| method.id.0.to_string()));
        let method = method
            .filter(|id| methods.iter().any(|method| method.id.0.as_str() == id))
            .ok_or_else(|| {
                safe_error(
                    &inner.instance,
                    "start",
                    "The provider did not advertise this sign-in method.",
                )
            })?;
        let stopped = (stop_routed.0)().await;
        stop_sessions(&inner).await;
        stopped?;
        inner
            .backend
            .authenticate(
                method,
                AuthContext {
                    inner: Arc::downgrade(&inner),
                    flow_id: id.clone(),
                    expires_at,
                    return_url: return_url.filter(|url| !url.is_empty()),
                    callback_mode,
                },
            )
            .await
    };
    let result = tokio::select! {biased;result=authenticate=>result,_=cancellation.wait_for(|cancel|*cancel)=>Err(safe_error(&inner.instance,"start","Sign-in cancelled.")),_=tokio::time::sleep(inner.timeout)=>Err(safe_error(&inner.instance,"start","Sign-in expired. Start again."))};
    inner.backend.cleanup().await;
    let response = {
        let _admission = tokio::select! {biased;admission=inner.admission.lock()=>admission,_=cancellation.wait_for(|cancel|*cancel)=>return};
        let mut state = inner.state.lock().unwrap();
        if let Some(flow) = state.active.as_mut().filter(|flow| flow.id == id) {
            let response = flow.response.take();
            state.operation = Operation::Stopping;
            response
        } else {
            None
        }
    };
    if let Some(response) = response {
        response.stop().await;
    }
    if inner.refresh_after_auth {
        let result = tokio::select! {biased;result=inner.backend.methods()=>Some(result),_=cancellation.wait_for(|cancel|*cancel)=>None};
        inner.backend.cleanup_methods().await;
        let Some(result) = result else { return };
        publish_methods(&inner, result);
    }
    let _admission = tokio::select! {biased;admission=inner.admission.lock()=>admission,_=cancellation.wait_for(|cancel|*cancel)=>return};
    let mut state = inner.state.lock().unwrap();
    if state.active.as_ref().is_some_and(|flow| flow.id == id) {
        state.snapshot.phase = if result.is_ok() {
            ProviderAuthPhase::Succeeded
        } else {
            ProviderAuthPhase::Failed
        };
        state.snapshot.interaction = Some(Some(None));
        state.snapshot.authorization_url = None;
        state.snapshot.expires_at = None;
        state.snapshot.message = Some(match result {
            Ok(()) => "Sign-in complete.".into(),
            Err(error) => error.detail,
        });
        state.active = None;
        state.operation = Operation::Idle;
        state.publish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::oneshot;
    enum Event {
        Authenticate(AuthContext, oneshot::Sender<AuthResult<()>>),
        Methods(oneshot::Sender<()>),
        Cleanup(&'static str, oneshot::Sender<()>),
    }
    struct Backend {
        events: mpsc::UnboundedSender<Event>,
        held_methods: Arc<AtomicBool>,
        held_cleanup: bool,
        held_methods_cleanup: bool,
    }
    impl AuthBackend for Backend {
        fn methods(&self) -> BoxFuture<'static, AuthResult<Vec<ProviderAuthMethod>>> {
            let events = self.events.clone();
            let held = self.held_methods.clone();
            Box::pin(async move {
                if held.load(Ordering::SeqCst) {
                    let (sender, release) = oneshot::channel();
                    let _ = events.send(Event::Methods(sender));
                    let _ = release.await;
                }
                Ok(vec![serde_json::from_value(serde_json::json!({"id":"agent","name":"Agent","description":null,"type":"agent"})).unwrap()])
            })
        }
        fn authenticate(
            &self,
            _: String,
            context: AuthContext,
        ) -> BoxFuture<'static, AuthResult<()>> {
            let events = self.events.clone();
            Box::pin(async move {
                let (sender, result) = oneshot::channel();
                let _ = events.send(Event::Authenticate(context, sender));
                result.await.unwrap_or_else(|_| {
                    Err(safe_error(
                        "instance",
                        "start",
                        "Sign-in failed. Start again.",
                    ))
                })
            })
        }
        fn cleanup(&self) -> BoxFuture<'static, ()> {
            let events = self.events.clone();
            let held = self.held_cleanup;
            Box::pin(async move {
                if held {
                    let (sender, release) = oneshot::channel();
                    let _ = events.send(Event::Cleanup("auth", sender));
                    let _ = release.await;
                }
            })
        }
        fn cleanup_methods(&self) -> BoxFuture<'static, ()> {
            let events = self.events.clone();
            let held = self.held_methods_cleanup;
            Box::pin(async move {
                if held {
                    let (sender, release) = oneshot::channel();
                    let _ = events.send(Event::Cleanup("methods", sender));
                    let _ = release.await;
                }
            })
        }
        fn logout(&self) -> BoxFuture<'static, AuthResult<Option<String>>> {
            Box::pin(async { Ok(None) })
        }
    }
    fn noop() -> StopSession {
        Arc::new(|| Box::pin(async {}))
    }
    fn fixture(
        held_methods: bool,
        held_cleanup: bool,
        held_methods_cleanup: bool,
        refresh: bool,
    ) -> (AuthFlow, Arc<AtomicBool>, mpsc::UnboundedReceiver<Event>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let held = Arc::new(AtomicBool::new(held_methods));
        let backend = Backend {
            events: sender,
            held_methods: held.clone(),
            held_cleanup,
            held_methods_cleanup,
        };
        (
            AuthFlow::new(
                "instance".into(),
                Arc::new(backend),
                None,
                Duration::from_secs(300),
                ProviderCredentialOwner::Provider,
                refresh,
            ),
            held,
            receiver,
        )
    }
    async fn phase(
        subscription: &mut AuthSubscription,
        expected: ProviderAuthPhase,
    ) -> ProviderAuthState {
        loop {
            let state = subscription.recv().await.unwrap();
            if state.phase == expected {
                return state;
            }
        }
    }
    #[tokio::test]
    async fn owner_privacy_repeated_start_and_exact_response_drive_verification_and_success() {
        tokio::time::timeout(Duration::from_secs(5),async{
        let(flow,_,mut events)=fixture(false,false,false,false);let mut visible=flow.subscribe("owner".into());let mut foreign=flow.subscribe("other".into());
        let started=flow.start("owner".into(),None,None,None,noop()).await.unwrap();let id=started.flow_id.as_ref().unwrap().0.to_string();
        assert_eq!(flow.start("owner".into(),None,None,None,noop()).await.unwrap().flow_id,started.flow_id);
        assert!(flow.start("other".into(),None,None,None,noop()).await.is_err());
        let Event::Authenticate(context,result)=events.recv().await.unwrap() else{panic!("authenticate")};
        let result=Arc::new(Mutex::new(Some(result)));let verification=context.clone();
        context.set_interaction(serde_json::from_value(serde_json::json!({"type":"browser","id":"consent","url":"https://example.test/login","requiresConsent":true})).unwrap(),Some(Respond::new(Arc::new(move |response|{
            let result=result.clone();let context=verification.clone();Box::pin(async move{assert!(matches!(response,ProviderAuthResponse::Browser{action:ProviderAuthBrowserAction::Accept}));context.verifying();let _=result.lock().unwrap().take().unwrap().send(Ok(()));Ok(())})
        }))),None);
        let waiting=phase(&mut visible,ProviderAuthPhase::Waiting).await;assert_eq!(waiting.flow_id,started.flow_id);
        let hidden=phase(&mut foreign,ProviderAuthPhase::Waiting).await;assert_eq!(hidden.flow_id,None);assert_eq!(hidden.authorization_url,None);assert_eq!(hidden.expires_at,None);assert_eq!(hidden.interaction,Some(Some(None)));assert_eq!(hidden.message.as_deref(),Some("Sign-in is in progress in another client."));
        let input:ProviderAuthRespondInput=serde_json::from_value(serde_json::json!({"instanceId":"instance","flowId":id,"interactionId":"consent","response":{"type":"browser","action":"accept"}})).unwrap();
        assert!(flow.respond("other",input.clone()).await.is_err());flow.respond("owner",input).await.unwrap();
        let succeeded=phase(&mut visible,ProviderAuthPhase::Succeeded).await;assert_eq!(succeeded.flow_id,started.flow_id);assert_eq!(succeeded.expires_at,None);assert_eq!(succeeded.message.as_deref(),Some("Sign-in complete."));assert!(!flow.is_changing_credentials());flow.shutdown().await;
    }).await.unwrap();
    }
    #[tokio::test]
    async fn cancellation_waits_owned_auth_cleanup_before_idle_and_returns_unredacted_flow() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (flow, _, mut events) = fixture(false, true, false, false);
            let started = flow
                .start("owner".into(), None, None, None, noop())
                .await
                .unwrap();
            let id = started.flow_id.as_ref().unwrap().0.to_string();
            let Event::Authenticate(_, _) = events.recv().await.unwrap() else {
                panic!("authenticate")
            };
            let cancel_flow = flow.clone();
            let cancel = tokio::spawn(async move { cancel_flow.cancel("owner", &id).await });
            let Event::Cleanup("auth", release) = events.recv().await.unwrap() else {
                panic!("auth cleanup")
            };
            assert!(!cancel.is_finished());
            assert!(flow.admit_session(noop()).await.is_err());
            assert!(flow.is_changing_credentials());
            release.send(()).unwrap();
            let cancelled = cancel.await.unwrap().unwrap();
            assert_eq!(cancelled.flow_id, started.flow_id);
            assert_eq!(cancelled.phase, ProviderAuthPhase::Cancelled);
            assert!(!flow.is_changing_credentials());
            flow.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn controller_shutdown_cancels_initial_discovery_and_waits_its_cleanup() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (flow, _, mut events) = fixture(true, false, true, false);
            let Event::Methods(_held) = events.recv().await.unwrap() else {
                panic!("methods")
            };
            let stopping = flow.clone();
            let shutdown = tokio::spawn(async move { stopping.shutdown().await });
            let Event::Cleanup("methods", release) = events.recv().await.unwrap() else {
                panic!("methods cleanup")
            };
            assert!(!shutdown.is_finished());
            release.send(()).unwrap();
            shutdown.await.unwrap();
            assert!(flow.is_changing_credentials());
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn successful_access_is_retained_by_session_parent_and_survives_controller_rebuild() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (flow, _, _) = fixture(false, false, false, false);
            let stops = Arc::new(AtomicUsize::new(0));
            let stop_count = stops.clone();
            let access = flow
                .admit_session(Arc::new(move || {
                    let count = stop_count.clone();
                    Box::pin(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                    })
                }))
                .await
                .unwrap();
            flow.shutdown().await;
            assert_eq!(stops.load(Ordering::SeqCst), 0);
            access.dispose().await;
            assert_eq!(stops.load(Ordering::SeqCst), 1);
            access.dispose().await;
            assert_eq!(stops.load(Ordering::SeqCst), 1);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn routed_stop_failure_still_closes_owned_scopes_and_never_authenticates() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (flow, _, mut events) = fixture(false, false, false, false);
            let mut states = flow.subscribe("owner".into());
            while states.recv().await.unwrap().methods.is_none() {}
            let count = Arc::new(AtomicUsize::new(0));
            let stopping = count.clone();
            let access = flow
                .admit_session(Arc::new(move || {
                    let stopping = stopping.clone();
                    Box::pin(async move {
                        stopping.fetch_add(1, Ordering::SeqCst);
                    })
                }))
                .await
                .unwrap();
            let failed = || {
                RoutedStop::new(Arc::new(|| {
                    Box::pin(async {
                        Err(safe_error(
                            "instance",
                            "stopSessions",
                            "Could not stop all sessions for this provider. Try again.",
                        ))
                    })
                }))
            };
            flow.start("owner".into(), None, None, None, failed())
                .await
                .unwrap();
            let state = phase(&mut states, ProviderAuthPhase::Failed).await;
            assert_eq!(
                state.message.as_deref(),
                Some("Could not stop all sessions for this provider. Try again.")
            );
            assert_eq!(count.load(Ordering::SeqCst), 1);
            assert!(
                events.try_recv().is_err(),
                "failed routed cleanup must not start authentication"
            );
            access.dispose().await;
            assert_eq!(count.load(Ordering::SeqCst), 1);
            let error = flow.logout(failed()).await.unwrap_err();
            assert_eq!(error.operation, "stopSessions");
            assert_eq!(flow.snapshot("owner").phase, ProviderAuthPhase::Failed);
            assert_eq!(
                flow.snapshot("owner").message.as_deref(),
                Some("Could not sign out. Try again.")
            );
            flow.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn shared_invalidation_retains_admission_until_owned_scope_cleanup_after_caller_drop() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (flow, _, _) = fixture(false, false, false, false);
            let mut states = flow.subscribe("owner".into());
            while states.recv().await.unwrap().methods.is_none() {}
            let (entered, entry) = oneshot::channel();
            let (release, held) = oneshot::channel();
            let signals = Arc::new(Mutex::new(Some((entered, held))));
            let access = flow
                .admit_session(Arc::new(move || {
                    let (entered, held) = signals.lock().unwrap().take().unwrap();
                    Box::pin(async move {
                        let _ = entered.send(());
                        let _ = held.await;
                    })
                }))
                .await
                .unwrap();
            let invalidating = flow.clone();
            let task = tokio::spawn(async move { invalidating.invalidate().await });
            entry.await.unwrap();
            task.abort();
            let _ = task.await;
            let admission = flow.admit_session(noop());
            tokio::pin!(admission);
            assert!(futures_util::poll!(&mut admission).is_pending());
            release.send(()).unwrap();
            access.dispose().await;
            let next = admission.await.unwrap();
            let state = flow.snapshot("owner");
            assert_eq!(
                state.message.as_deref(),
                Some("This provider's shared sign-in changed.")
            );
            assert_eq!(state.phase, ProviderAuthPhase::Idle);
            assert_eq!(state.methods.unwrap().0.len(), 1);
            next.dispose().await;
            flow.shutdown().await;
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn shutdown_interrupts_held_postauth_refresh_and_waits_methods_cleanup() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (flow, held, mut events) = fixture(false, false, true, true);
            // Initial refresh has a scoped cleanup barrier too.
            let Event::Cleanup("methods", release) = events.recv().await.unwrap() else {
                panic!("initial methods cleanup")
            };
            release.send(()).unwrap();
            let mut states = flow.subscribe("owner".into());
            loop {
                if states.recv().await.unwrap().methods.is_some() {
                    break;
                }
            }
            flow.start("owner".into(), None, None, None, noop())
                .await
                .unwrap();
            let Event::Authenticate(_, result) = events.recv().await.unwrap() else {
                panic!("authenticate")
            };
            held.store(true, Ordering::SeqCst);
            result.send(Ok(())).unwrap();
            let Event::Methods(_held) = events.recv().await.unwrap() else {
                panic!("postauth methods")
            };
            let stopping = flow.clone();
            let shutdown = tokio::spawn(async move { stopping.shutdown().await });
            let Event::Cleanup("methods", release) = events.recv().await.unwrap() else {
                panic!("postauth cleanup")
            };
            assert!(!shutdown.is_finished());
            release.send(()).unwrap();
            shutdown.await.unwrap();
        })
        .await
        .unwrap();
    }
}
