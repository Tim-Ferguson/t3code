//! Environment-wide ACP startup priority, native-session admission and live state.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use t3_contracts::{
    AcpRegistryAcceptUrlAuthInput, AcpRegistryProbeModel, AcpRegistryUrlAuthAction, BoundedString,
    ProviderOptionDescriptor, ServerProviderSkill, ServerProviderSlashCommand,
};
use tokio::sync::{OwnedMutexGuard, mpsc, oneshot};
const URL_AUTH_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AvailableCommands {
    pub slash_commands: Vec<ServerProviderSlashCommand>,
    pub skills: Vec<ServerProviderSkill>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LiveConfiguration {
    pub models: Vec<AcpRegistryProbeModel>,
    pub current_model_id: Option<String>,
    pub config_options: Vec<ProviderOptionDescriptor>,
}
#[derive(Clone)]
pub enum LiveUpdate {
    Commands(AvailableCommands),
    Configuration(LiveConfiguration),
    UrlAction(Option<AcpRegistryUrlAuthAction>),
}
type LiveCallback = Arc<dyn Fn(&str, &LiveUpdate) + Send + Sync>;
#[derive(Default)]
struct Observers {
    next: u64,
    callbacks: HashMap<u64, LiveCallback>,
}
pub struct LiveObserver {
    observers: Weak<Mutex<Observers>>,
    id: u64,
}
impl Drop for LiveObserver {
    fn drop(&mut self) {
        if let Some(observers) = self.observers.upgrade() {
            observers.lock().unwrap().callbacks.remove(&self.id);
        }
    }
}
fn notify(observers: &Arc<Mutex<Observers>>, instance: &str, update: LiveUpdate) {
    let callbacks = observers
        .lock()
        .unwrap()
        .callbacks
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for callback in callbacks {
        callback(instance, &update);
    }
}
struct Topic<T> {
    values: HashMap<String, T>,
    senders: HashMap<u64, (String, mpsc::UnboundedSender<T>)>,
    next_id: u64,
}
impl<T> Default for Topic<T> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            senders: HashMap::new(),
            next_id: 0,
        }
    }
}
impl<T: Clone> Topic<T> {
    fn publish(&mut self, key: &str, value: T) {
        self.senders
            .retain(|_, (instance, sender)| instance != key || sender.send(value.clone()).is_ok());
    }
    fn set(&mut self, key: &str, value: T) {
        self.values.insert(key.into(), value.clone());
        self.publish(key, value);
    }
}
pub struct StateSubscription<T> {
    receiver: mpsc::UnboundedReceiver<T>,
    topic: Weak<Mutex<Topic<T>>>,
    id: u64,
}
impl<T> StateSubscription<T> {
    pub async fn recv(&mut self) -> Option<T> {
        self.receiver.recv().await
    }
}
impl<T> Drop for StateSubscription<T> {
    fn drop(&mut self) {
        if let Some(topic) = self.topic.upgrade() {
            topic.lock().unwrap().senders.remove(&self.id);
        }
    }
}
fn subscribe<T: Clone>(topic: &Arc<Mutex<Topic<T>>>, key: &str) -> StateSubscription<T> {
    let mut state = topic.lock().unwrap();
    let (sender, receiver) = mpsc::unbounded_channel();
    if let Some(current) = state.values.get(key) {
        let _ = sender.send(current.clone());
    }
    let id = state.next_id;
    state.next_id += 1;
    state.senders.insert(id, (key.into(), sender));
    StateSubscription {
        receiver,
        topic: Arc::downgrade(topic),
        id,
    }
}
#[derive(Default)]
struct ForegroundState {
    counts: HashMap<String, usize>,
    probes: HashMap<u64, (String, mpsc::UnboundedSender<()>)>,
    next_id: u64,
}
pub struct ForegroundStartup {
    state: Weak<Mutex<ForegroundState>>,
    key: String,
}
impl Drop for ForegroundStartup {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap();
            let count = state.counts.get_mut(&self.key).unwrap();
            *count -= 1;
            if *count == 0 {
                state.counts.remove(&self.key);
            }
        }
    }
}
/// Register before resolving or spawning a disposable provider process.
pub struct BackgroundProbe {
    state: Weak<Mutex<ForegroundState>>,
    receiver: mpsc::UnboundedReceiver<()>,
    id: u64,
}
impl BackgroundProbe {
    pub async fn interrupted(&mut self) {
        let _ = self.receiver.recv().await;
    }
}
impl Drop for BackgroundProbe {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.lock().unwrap().probes.remove(&self.id);
        }
    }
}
struct UrlRequest {
    token: u64,
    action: AcpRegistryUrlAuthAction,
    expires_at: DateTime<Utc>,
    consent: Option<oneshot::Sender<bool>>,
}
#[derive(Default)]
struct UrlState {
    requests: HashMap<String, UrlRequest>,
    next_token: u64,
}
type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;
#[derive(Clone)]
pub struct Coordinator {
    foreground: Arc<Mutex<ForegroundState>>,
    session_mutation: Arc<tokio::sync::Mutex<()>>,
    commands: Arc<Mutex<Topic<AvailableCommands>>>,
    configuration: Arc<Mutex<Topic<LiveConfiguration>>>,
    url_state: Arc<Mutex<UrlState>>,
    url_updates: Arc<Mutex<Topic<Option<AcpRegistryUrlAuthAction>>>>,
    now: Clock,
    observers: Arc<Mutex<Observers>>,
    publication: Arc<Mutex<()>>,
}
impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator").finish_non_exhaustive()
    }
}
impl Default for Coordinator {
    fn default() -> Self {
        Self::with_clock(Arc::new(Utc::now))
    }
}
impl Coordinator {
    fn with_clock(now: Clock) -> Self {
        Self {
            foreground: Default::default(),
            session_mutation: Default::default(),
            commands: Default::default(),
            configuration: Default::default(),
            url_state: Default::default(),
            url_updates: Default::default(),
            now,
            observers: Default::default(),
            publication: Default::default(),
        }
    }
    pub(crate) fn observe_live_state(&self, callback: LiveCallback) -> LiveObserver {
        let mut observers = self.observers.lock().unwrap();
        let id = observers.next;
        observers.next += 1;
        observers.callbacks.insert(id, callback);
        LiveObserver {
            observers: Arc::downgrade(&self.observers),
            id,
        }
    }
    pub fn foreground_startup(&self, key: &str) -> ForegroundStartup {
        let mut state = self.foreground.lock().unwrap();
        *state.counts.entry(key.into()).or_default() += 1;
        state
            .probes
            .retain(|_, (agent, sender)| agent != key || sender.send(()).is_ok());
        ForegroundStartup {
            state: Arc::downgrade(&self.foreground),
            key: key.into(),
        }
    }
    pub fn background_probe(&self, key: &str) -> Option<BackgroundProbe> {
        let mut state = self.foreground.lock().unwrap();
        if state.counts.contains_key(key) {
            return None;
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        let id = state.next_id;
        state.next_id += 1;
        state.probes.insert(id, (key.into(), sender));
        Some(BackgroundProbe {
            state: Arc::downgrade(&self.foreground),
            receiver,
            id,
        })
    }
    /// Caller retains this guard through the complete native import/delete.
    pub async fn session_mutation(&self) -> OwnedMutexGuard<()> {
        self.session_mutation.clone().lock_owned().await
    }
    pub fn clear_commands(&self, instance: &str) {
        self.commands.lock().unwrap().values.remove(instance);
    }
    pub fn publish_commands(&self, instance: &str, commands: AvailableCommands) {
        let _publication = self.publication.lock().unwrap();
        self.commands
            .lock()
            .unwrap()
            .set(instance, commands.clone());
        notify(&self.observers, instance, LiveUpdate::Commands(commands));
    }
    pub fn commands(&self, instance: &str) -> Option<AvailableCommands> {
        self.commands.lock().unwrap().values.get(instance).cloned()
    }
    pub fn subscribe_commands(&self, instance: &str) -> StateSubscription<AvailableCommands> {
        subscribe(&self.commands, instance)
    }
    pub fn clear_configuration(&self, instance: &str) {
        self.configuration.lock().unwrap().values.remove(instance);
    }
    pub fn publish_configuration(&self, instance: &str, configuration: LiveConfiguration) {
        let _publication = self.publication.lock().unwrap();
        self.configuration
            .lock()
            .unwrap()
            .set(instance, configuration.clone());
        notify(
            &self.observers,
            instance,
            LiveUpdate::Configuration(configuration),
        );
    }
    pub fn configuration(&self, instance: &str) -> Option<LiveConfiguration> {
        self.configuration
            .lock()
            .unwrap()
            .values
            .get(instance)
            .cloned()
    }
    pub fn subscribe_configuration(&self, instance: &str) -> StateSubscription<LiveConfiguration> {
        subscribe(&self.configuration, instance)
    }
    pub fn url_action(&self, instance: &str) -> Option<AcpRegistryUrlAuthAction> {
        self.url_state
            .lock()
            .unwrap()
            .requests
            .get(instance)
            .map(|request| request.action.clone())
    }
    pub fn subscribe_url_action(
        &self,
        instance: &str,
    ) -> StateSubscription<Option<AcpRegistryUrlAuthAction>> {
        // The same lock order is used for publication and cleanup, closing the
        // snapshot/subscription gap without holding a callback under a mutex.
        let _state = self.url_state.lock().unwrap();
        subscribe(&self.url_updates, instance)
    }
    pub async fn request_url_authentication(
        &self,
        instance: &str,
        mut action: AcpRegistryUrlAuthAction,
    ) -> bool {
        let now = (self.now)();
        let expires_at = now + chrono::Duration::from_std(URL_AUTH_TTL).unwrap();
        action.created_at = Some(BoundedString(
            now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ));
        action.expires_at = Some(BoundedString(
            expires_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        ));
        let (sender, receiver) = oneshot::channel();
        let token = {
            let _publication = self.publication.lock().unwrap();
            let mut state = self.url_state.lock().unwrap();
            let token = state.next_token;
            state.next_token += 1;
            let previous = state.requests.insert(
                instance.into(),
                UrlRequest {
                    token,
                    action: action.clone(),
                    expires_at,
                    consent: Some(sender),
                },
            );
            let mut updates = self.url_updates.lock().unwrap();
            updates.set(instance, Some(action.clone()));
            if let Some(previous) = previous {
                if let Some(consent) = previous.consent {
                    let _ = consent.send(false);
                }
            }
            drop(updates);
            drop(state);
            notify(
                &self.observers,
                instance,
                LiveUpdate::UrlAction(Some(action)),
            );
            token
        };
        let _cleanup = UrlCleanup {
            observers: Arc::downgrade(&self.observers),
            publication: Arc::downgrade(&self.publication),
            state: Arc::downgrade(&self.url_state),
            updates: Arc::downgrade(&self.url_updates),
            instance: instance.into(),
            token,
        };
        tokio::time::timeout(URL_AUTH_TTL, receiver)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false)
    }
    pub fn accept_url_authentication(&self, input: &AcpRegistryAcceptUrlAuthInput) -> bool {
        let mut state = self.url_state.lock().unwrap();
        let Some(request) = state.requests.get_mut(input.instance_id.as_str()) else {
            return false;
        };
        if request.action.elicitation_id.0.as_str() != input.elicitation_id.0.as_str() {
            return false;
        }
        if (self.now)() >= request.expires_at {
            if let Some(consent) = request.consent.take() {
                let _ = consent.send(false);
            }
            return false;
        }
        request
            .consent
            .take()
            .is_some_and(|consent| consent.send(true).is_ok())
    }
}
struct UrlCleanup {
    publication: Weak<Mutex<()>>,
    observers: Weak<Mutex<Observers>>,
    state: Weak<Mutex<UrlState>>,
    updates: Weak<Mutex<Topic<Option<AcpRegistryUrlAuthAction>>>>,
    instance: String,
    token: u64,
}
impl Drop for UrlCleanup {
    fn drop(&mut self) {
        let publication = self.publication.upgrade();
        let _publication = publication
            .as_ref()
            .map(|publication| publication.lock().unwrap());
        let mut cleared = false;
        if let Some(state) = self.state.upgrade() {
            let mut state = state.lock().unwrap();
            if state
                .requests
                .get(&self.instance)
                .is_some_and(|request| request.token == self.token)
            {
                state.requests.remove(&self.instance);
                if let Some(updates) = self.updates.upgrade() {
                    let mut updates = updates.lock().unwrap();
                    updates.values.remove(&self.instance);
                    updates.publish(&self.instance, None);
                }
                cleared = true;
            }
        }
        if cleared {
            if let Some(observers) = self.observers.upgrade() {
                notify(&observers, &self.instance, LiveUpdate::UrlAction(None));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn action(id: &str) -> AcpRegistryUrlAuthAction {
        serde_json::from_value(serde_json::json!({"elicitationId":id,"url":"https://accounts.example.com/login","message":"Continue in your browser"})).unwrap()
    }
    fn accept(instance: &str, id: &str) -> AcpRegistryAcceptUrlAuthInput {
        serde_json::from_value(serde_json::json!({"instanceId":instance,"elicitationId":id}))
            .unwrap()
    }
    #[tokio::test]
    async fn foreground_priority_interrupts_only_matching_probes_and_nested_startups_hold_admission()
     {
        let coordinator = Coordinator::default();
        let mut matching = coordinator.background_probe("agent").unwrap();
        let mut other = coordinator.background_probe("other").unwrap();
        let first = coordinator.foreground_startup("agent");
        matching.interrupted().await;
        assert!(coordinator.background_probe("agent").is_none());
        assert!(other.receiver.try_recv().is_err());
        let second = coordinator.foreground_startup("agent");
        drop(first);
        assert!(coordinator.background_probe("agent").is_none());
        drop(second);
        assert!(coordinator.background_probe("agent").is_some());
        drop(matching);
        drop(other);
        assert!(coordinator.foreground.lock().unwrap().probes.is_empty());
    }
    #[tokio::test]
    async fn native_mutations_share_admission_and_readonly_live_state_remains_available() {
        let coordinator = Coordinator::default();
        let guard = coordinator.session_mutation().await;
        let (entered, mut observed) = mpsc::unbounded_channel();
        let cloned = coordinator.clone();
        let queued = tokio::spawn(async move {
            let _guard = cloned.session_mutation().await;
            entered.send(()).unwrap();
        });
        assert!(coordinator.session_mutation.try_lock().is_err());
        coordinator.publish_commands("a", AvailableCommands::default());
        assert!(coordinator.commands("a").is_some());
        drop(guard);
        observed.recv().await.unwrap();
        queued.await.unwrap();
    }
    #[tokio::test]
    async fn late_streams_replay_and_buffer_replacements_per_instance_and_drop_own_subscription() {
        let coordinator = Coordinator::default();
        coordinator.publish_commands("a", AvailableCommands::default());
        let mut stream = coordinator.subscribe_commands("a");
        assert_eq!(stream.recv().await.unwrap(), AvailableCommands::default());
        let replacement: AvailableCommands = serde_json::from_value(
            serde_json::json!({"slashCommands":[{"name":"help"}],"skills":[]}),
        )
        .unwrap();
        coordinator.publish_commands("other", AvailableCommands::default());
        coordinator.publish_commands("a", replacement.clone());
        assert_eq!(stream.recv().await.unwrap(), replacement);
        coordinator.clear_commands("a");
        assert!(coordinator.commands("a").is_none());
        assert!(
            stream.receiver.try_recv().is_err(),
            "source clear does not publish"
        );
        drop(stream);
        assert!(coordinator.commands.lock().unwrap().senders.is_empty());
        coordinator.publish_configuration("a", LiveConfiguration::default());
        let mut stream = coordinator.subscribe_configuration("a");
        assert_eq!(stream.recv().await.unwrap(), LiveConfiguration::default());
        let replacement = LiveConfiguration {
            current_model_id: Some("model".into()),
            ..Default::default()
        };
        coordinator.publish_configuration("a", replacement.clone());
        assert_eq!(stream.recv().await.unwrap(), replacement);
        coordinator.clear_configuration("a");
        assert!(coordinator.configuration("a").is_none());
    }
    #[tokio::test]
    async fn matching_user_consent_is_single_use_and_request_completion_clears_published_action() {
        let coordinator = Coordinator::default();
        let mut updates = coordinator.subscribe_url_action("instance");
        assert!(
            updates.receiver.try_recv().is_err(),
            "no empty snapshot seed"
        );
        let cloned = coordinator.clone();
        let request = tokio::spawn(async move {
            cloned
                .request_url_authentication("instance", action("login"))
                .await
        });
        let published = updates.recv().await.unwrap().unwrap();
        assert!(published.created_at.is_some());
        assert!(published.expires_at.is_some());
        let mut late = coordinator.subscribe_url_action("instance");
        assert_eq!(late.recv().await.unwrap(), Some(published));
        assert!(!coordinator.accept_url_authentication(&accept("other", "login")));
        assert!(!coordinator.accept_url_authentication(&accept("instance", "wrong")));
        assert!(coordinator.accept_url_authentication(&accept("instance", "login")));
        assert!(!coordinator.accept_url_authentication(&accept("instance", "login")));
        assert!(request.await.unwrap());
        assert_eq!(updates.recv().await.unwrap(), None);
        assert!(coordinator.url_action("instance").is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn url_expiry_replacement_and_interruption_cannot_clear_a_new_request() {
        let initial = Utc::now();
        let epoch = tokio::time::Instant::now();
        let coordinator = Coordinator::with_clock(Arc::new(move || {
            initial + chrono::Duration::from_std(epoch.elapsed()).unwrap()
        }));
        let mut updates = coordinator.subscribe_url_action("instance");
        let cloned = coordinator.clone();
        let first = tokio::spawn(async move {
            cloned
                .request_url_authentication("instance", action("first"))
                .await
        });
        assert_eq!(
            updates
                .recv()
                .await
                .unwrap()
                .unwrap()
                .elicitation_id
                .0
                .as_str(),
            "first"
        );
        let cloned = coordinator.clone();
        let second = tokio::spawn(async move {
            cloned
                .request_url_authentication("instance", action("second"))
                .await
        });
        assert_eq!(
            updates
                .recv()
                .await
                .unwrap()
                .unwrap()
                .elicitation_id
                .0
                .as_str(),
            "second"
        );
        assert!(!first.await.unwrap());
        assert_eq!(
            coordinator
                .url_action("instance")
                .unwrap()
                .elicitation_id
                .0
                .as_str(),
            "second"
        );
        tokio::time::advance(URL_AUTH_TTL).await;
        assert!(!coordinator.accept_url_authentication(&accept("instance", "second")));
        assert!(!second.await.unwrap());
        assert_eq!(updates.recv().await.unwrap(), None);
        let cloned = coordinator.clone();
        let interrupted = tokio::spawn(async move {
            cloned
                .request_url_authentication("instance", action("third"))
                .await
        });
        assert_eq!(
            updates
                .recv()
                .await
                .unwrap()
                .unwrap()
                .elicitation_id
                .0
                .as_str(),
            "third"
        );
        interrupted.abort();
        assert!(interrupted.await.unwrap_err().is_cancelled());
        assert_eq!(updates.recv().await.unwrap(), None);
        assert!(coordinator.url_action("instance").is_none());
    }
}
