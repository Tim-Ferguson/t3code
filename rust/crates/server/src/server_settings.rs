//! Serialized settings ownership. Once a transaction begins, its blocking owner
//! completes secrets, disk and publication even if the request future is dropped.
use crate::{
    server_secret_store::{ServerSecretStore, write_string_atomically},
    server_settings_model::{
        apply_patch, normalize_settings, resolve_text_generation, sparse_settings,
    },
    server_settings_secrets::{AppliedSecrets, MaterializeError, SecretBackend, materialize, plan},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};
use t3_contracts::{ProviderInstanceMutation, ServerSettings, ServerSettingsPatch};
use tokio::sync::{Mutex, mpsc, oneshot};

#[derive(Debug, thiserror::Error)]
#[error("Server settings {operation} failed at {settings_path}.")]
pub struct SettingsError {
    pub settings_path: PathBuf,
    pub operation: &'static str,
    #[source]
    pub cause: Option<Box<dyn std::error::Error + Send + Sync>>,
}
impl SettingsError {
    fn new(
        path: &Path,
        operation: &'static str,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            settings_path: path.into(),
            operation,
            cause: Some(Box::new(cause)),
        }
    }
    pub(crate) fn stopped(path: &Path) -> Self {
        Self {
            settings_path: path.into(),
            operation: "read-file",
            cause: None,
        }
    }
    pub fn wire(&self) -> Value {
        // Never include the original file/patch/secret bytes in a transport error.
        json!({"_tag":"ServerSettingsError","settingsPath":self.settings_path,"operation":self.operation})
    }
}
pub trait SettingsWriter: Send + Sync {
    fn write(&self, path: &Path, contents: &str) -> std::io::Result<()>;
}
pub struct AtomicSettingsWriter;
impl SettingsWriter for AtomicSettingsWriter {
    fn write(&self, path: &Path, contents: &str) -> std::io::Result<()> {
        write_string_atomically(path, contents)
    }
}
pub struct SettingsOptions {
    pub path: PathBuf,
    pub secrets: Arc<dyn SecretBackend>,
    pub writer: Arc<dyn SettingsWriter>,
    pub watch: bool,
}
impl SettingsOptions {
    pub fn file(path: PathBuf, secrets: ServerSecretStore) -> Self {
        Self {
            path,
            secrets: Arc::new(secrets),
            writer: Arc::new(AtomicSettingsWriter),
            watch: true,
        }
    }
}
type Reply<T> = oneshot::Sender<Result<T, SettingsError>>;
enum Command {
    Snapshot(Reply<ServerSettings>),
    Update(
        Box<ServerSettingsPatch>,
        Option<ProviderInstanceMutation>,
        Reply<ServerSettings>,
    ),
    Reload(Reply<ServerSettings>),
    Subscribe(Reply<SettingsSubscription>),
    FileChanged,
    Shutdown,
}
pub struct SettingsSubscription {
    pub snapshot: ServerSettings,
    updates: mpsc::UnboundedReceiver<ServerSettings>,
    secrets: Arc<dyn SecretBackend>,
    pending: Option<tokio::task::JoinHandle<ServerSettings>>,
}
impl SettingsSubscription {
    pub async fn recv(&mut self) -> Option<ServerSettings> {
        if self.pending.is_none() {
            let persisted = self.updates.recv().await?;
            let secrets = self.secrets.clone();
            self.pending = Some(tokio::task::spawn_blocking(move || {
                match materialize(&persisted, secrets.as_ref()) {
                    Ok(materialized) => resolve_text_generation(materialized),
                    Err(_) => {
                        tracing::warn!(
                            "failed to materialize settings change; retaining persisted snapshot"
                        );
                        resolve_text_generation(persisted)
                    }
                }
            }));
        }
        // A merge/select may stop polling this receive while another source is
        // ready. Retain the already-dequeued event and its materialization.
        let result = self.pending.as_mut().unwrap().await;
        self.pending = None;
        result.ok()
    }
}
struct Inner {
    path: PathBuf,
    sender: mpsc::UnboundedSender<Command>,
    stopped: AtomicBool,
    stop: tokio::sync::watch::Sender<bool>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    watch_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        let _ = self.sender.send(Command::Shutdown);
    }
}
#[derive(Clone)]
pub struct SettingsService {
    inner: Arc<Inner>,
}
struct StartupGuard(
    mpsc::UnboundedSender<Command>,
    bool,
    tokio::task::AbortHandle,
);
impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.1 {
            self.2.abort();
            let _ = self.0.send(Command::Shutdown);
        }
    }
}
impl SettingsService {
    pub fn settings_path(&self) -> &Path {
        &self.inner.path
    }
    pub async fn start(options: SettingsOptions) -> Result<Self, SettingsError> {
        let path = options.path.clone();
        let (sender, commands) = mpsc::unbounded_channel();
        let (events, mut observed) = mpsc::unbounded_channel();
        let watch_sender = sender.clone();
        let watch_task = tokio::spawn(async move {
            while observed.recv().await.is_some() {
                loop {
                    tokio::select! {
                        event=observed.recv() => { if event.is_none() { return; } },
                        _=tokio::time::sleep(std::time::Duration::from_millis(100)) => break,
                    }
                }
                if watch_sender.send(Command::FileChanged).is_err() {
                    return;
                }
            }
        });
        let mut guard = StartupGuard(sender.clone(), true, watch_task.abort_handle());
        let (ready, initialized) = oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || run(options, commands, ready, events));
        let result = initialized
            .await
            .map_err(|_| SettingsError::stopped(&path))?;
        result?;
        let service = Self {
            inner: Arc::new(Inner {
                path,
                sender,
                stopped: AtomicBool::new(false),
                stop: tokio::sync::watch::channel(false).0,
                worker: Mutex::new(Some(worker)),
                watch_task: Mutex::new(Some(watch_task)),
            }),
        };
        guard.1 = false;
        Ok(service)
    }
    async fn request<T>(
        &self,
        command: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<T, SettingsError> {
        if self.inner.stopped.load(Ordering::Acquire) {
            return Err(SettingsError::stopped(&self.inner.path));
        }
        let (reply, receive) = oneshot::channel();
        self.inner
            .sender
            .send(command(reply))
            .map_err(|_| SettingsError::stopped(&self.inner.path))?;
        receive
            .await
            .map_err(|_| SettingsError::stopped(&self.inner.path))?
    }
    pub async fn snapshot(&self) -> Result<ServerSettings, SettingsError> {
        self.request(Command::Snapshot).await
    }
    pub async fn update(
        &self,
        patch: ServerSettingsPatch,
    ) -> Result<ServerSettings, SettingsError> {
        self.request(|reply| Command::Update(Box::new(patch), None, reply))
            .await
    }
    pub async fn update_provider_instance(
        &self,
        mutation: ProviderInstanceMutation,
        patch: ServerSettingsPatch,
    ) -> Result<ServerSettings, SettingsError> {
        self.request(|reply| Command::Update(Box::new(patch), Some(mutation), reply))
            .await
    }
    pub async fn reload(&self) -> Result<ServerSettings, SettingsError> {
        self.request(Command::Reload).await
    }
    /// Subscription admission and the initial snapshot share the writer's queue.
    /// There is no gap between reading the snapshot and admitting later changes.
    pub async fn subscribe(&self) -> Result<SettingsSubscription, SettingsError> {
        self.request(Command::Subscribe).await
    }
    pub async fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        if !self.inner.stopped.swap(true, Ordering::AcqRel) {
            let _ = self.inner.sender.send(Command::Shutdown);
        }
        // Retain the handle if the first shutdown caller is cancelled while joining.
        let mut worker = self.inner.worker.lock().await;
        if let Some(handle) = worker.as_mut() {
            let _ = handle.await;
        }
        *worker = None;
        let mut watch = self.inner.watch_task.lock().await;
        if let Some(handle) = watch.as_mut() {
            let _ = handle.await;
        }
        *watch = None;
    }
    pub async fn closed(&self) {
        let mut stopped = self.inner.stop.subscribe();
        let _ = stopped.wait_for(|stopped| *stopped).await;
    }
}
struct Owner {
    options: SettingsOptions,
    cached: Option<ServerSettings>,
    listeners: Vec<mpsc::UnboundedSender<ServerSettings>>,
    watcher: Option<SettingsWatch>,
}
struct SettingsWatch {
    watcher: notify::RecommendedWatcher,
    targets: Arc<StdMutex<Vec<PathBuf>>>,
    directories: std::collections::HashSet<PathBuf>,
}
fn normalized_file(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .unwrap_or_else(|_| parent.to_path_buf())
            .join(name),
        _ => path.to_path_buf(),
    }
}
impl SettingsWatch {
    fn new(events: mpsc::UnboundedSender<()>) -> Result<Self, notify::Error> {
        let targets: Arc<StdMutex<Vec<PathBuf>>> = Arc::default();
        let matching = targets.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if let Ok(event) = event {
                let targets = matching.lock().unwrap();
                if event.paths.iter().any(|path| {
                    targets
                        .iter()
                        .any(|target| normalized_file(path) == *target)
                }) {
                    let _ = events.send(());
                }
            }
        })?;
        Ok(Self {
            watcher,
            targets,
            directories: std::collections::HashSet::new(),
        })
    }
    fn retarget(&mut self, path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use notify::Watcher;
        let target = crate::server_secret_store::resolve_symlink_target(path)?;
        let files = vec![normalized_file(path), normalized_file(&target)];
        let mut directories = std::collections::HashSet::new();
        for file in &files {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
                directories.insert(std::fs::canonicalize(parent)?);
            }
        }
        *self.targets.lock().unwrap() = files;
        for directory in directories.difference(&self.directories) {
            self.watcher
                .watch(directory, notify::RecursiveMode::NonRecursive)?;
        }
        for directory in self.directories.difference(&directories) {
            self.watcher.unwatch(directory)?;
        }
        self.directories = directories;
        Ok(())
    }
}
impl Owner {
    fn load(&mut self) -> Result<&ServerSettings, SettingsError> {
        if self.cached.is_none() {
            let settings = match std::fs::read(&self.options.path) {
                Ok(bytes) => match decode_settings(&bytes) {
                    Ok(settings) => settings,
                    Err(_) => {
                        // A malformed hand-edited file is untrusted: use defaults,
                        // without rewriting it or logging its potentially secret text.
                        tracing::warn!(path=?self.options.path,"failed to decode server settings; using defaults");
                        ServerSettings::default()
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    ServerSettings::default()
                }
                Err(error) => {
                    return Err(SettingsError::new(&self.options.path, "read-file", error));
                }
            };
            self.cached = Some(
                normalize_settings(&settings)
                    .map_err(|error| SettingsError::new(&self.options.path, "normalize", error))?,
            );
        }
        Ok(self.cached.as_ref().unwrap())
    }
    fn materialized(&mut self) -> Result<ServerSettings, SettingsError> {
        let settings = self.load()?.clone();
        materialize(&settings, self.options.secrets.as_ref())
            .map(resolve_text_generation)
            .map_err(|error| self.materialize_error(error))
    }
    fn materialize_error(&self, error: MaterializeError) -> SettingsError {
        match error {
            MaterializeError::Secret(error) => {
                SettingsError::new(&self.options.path, error.operation, error)
            }
            MaterializeError::Json(error) => {
                SettingsError::new(&self.options.path, "normalize", error)
            }
        }
    }
    fn publish(&mut self, snapshot: &ServerSettings) {
        self.listeners
            .retain(|listener| listener.send(snapshot.clone()).is_ok());
    }
    fn update(
        &mut self,
        patch: &ServerSettingsPatch,
        mutation: Option<ProviderInstanceMutation>,
    ) -> Result<ServerSettings, SettingsError> {
        let current = self.load()?.clone();
        if let Some(ProviderInstanceMutation::Create { instance_id, .. }) = &mutation {
            if current.provider_instances.contains_key(instance_id) {
                return Err(SettingsError {
                    settings_path: self.options.path.clone(),
                    operation: "create-provider-instance",
                    cause: None,
                });
            }
        }
        let mut updated = apply_patch(&current, patch)
            .map_err(|error| SettingsError::new(&self.options.path, "normalize", error))?;
        match mutation {
            Some(ProviderInstanceMutation::Create {
                instance_id,
                instance,
            })
            | Some(ProviderInstanceMutation::Upsert {
                instance_id,
                instance,
            }) => {
                updated.provider_instances.insert(instance_id, instance);
            }
            Some(ProviderInstanceMutation::Remove { instance_id }) => {
                updated.provider_instances.remove(&instance_id);
            }
            None => {}
        }
        let (next, changes) = plan(&current, &updated)
            .map_err(|error| SettingsError::new(&self.options.path, "normalize", error))?;
        let next = normalize_settings(&next)
            .map_err(|error| SettingsError::new(&self.options.path, "normalize", error))?;
        let guard = AppliedSecrets::apply(self.options.secrets.clone(), changes)
            .map_err(|error| SettingsError::new(&self.options.path, error.operation, error))?;
        let materialized = materialize(&next, self.options.secrets.as_ref())
            .map_err(|error| self.materialize_error(error))?;
        let value = sparse_settings(&next)
            .map_err(|error| SettingsError::new(&self.options.path, "normalize", error))?;
        let contents = serde_json::to_string_pretty(&value)
            .map_err(|error| SettingsError::new(&self.options.path, "normalize", error))?
            + "\n";
        self.options
            .writer
            .write(&self.options.path, &contents)
            .map_err(|error| SettingsError::new(&self.options.path, "write-file", error))?;
        // This owner is not tied to the caller's future. No cancellation point
        // separates the landed file, secret commit, cached state and notification.
        guard.commit();
        self.cached = Some(next.clone());
        self.publish(&next);
        Ok(resolve_text_generation(materialized))
    }
}
fn run(
    options: SettingsOptions,
    mut commands: mpsc::UnboundedReceiver<Command>,
    ready: Reply<()>,
    events: mpsc::UnboundedSender<()>,
) {
    let mut owner = Owner {
        options,
        cached: None,
        listeners: Vec::new(),
        watcher: None,
    };
    if let Some(parent) = owner.options.path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            let _ = ready.send(Err(SettingsError::new(
                &owner.options.path,
                "read-file",
                error,
            )));
            return;
        }
    }
    let result = (|| {
        if owner.options.watch {
            let mut watcher = SettingsWatch::new(events).map_err(|error| {
                SettingsError::new(&owner.options.path, "prepare-directory", error)
            })?;
            watcher
                .retarget(&owner.options.path)
                .map_err(|cause| SettingsError {
                    settings_path: owner.options.path.clone(),
                    operation: "prepare-directory",
                    cause: Some(cause),
                })?;
            owner.watcher = Some(watcher);
        }
        owner.load().map(|_| ())
    })();
    if ready.send(result).is_err() {
        return;
    }
    while let Some(command) = commands.blocking_recv() {
        match command {
            Command::Snapshot(reply) => {
                if !reply.is_closed() {
                    let result = owner.materialized();
                    let _ = reply.send(result);
                }
            }
            Command::Update(patch, mutation, reply) => {
                if !reply.is_closed() {
                    let result = owner.update(&patch, mutation);
                    let _ = reply.send(result);
                }
            }
            Command::Reload(reply) => {
                if !reply.is_closed() {
                    owner.cached = None;
                    let result = owner.materialized();
                    if result.is_ok() {
                        let persisted = owner.cached.as_ref().unwrap().clone();
                        owner.publish(&persisted);
                    }
                    let _ = reply.send(result);
                }
            }
            Command::Subscribe(reply) => {
                if !reply.is_closed() {
                    let result = owner.materialized().map(|snapshot| {
                        let (sender, updates) = mpsc::unbounded_channel();
                        owner.listeners.push(sender);
                        SettingsSubscription {
                            snapshot,
                            updates,
                            secrets: owner.options.secrets.clone(),
                            pending: None,
                        }
                    });
                    let _ = reply.send(result);
                }
            }
            Command::FileChanged => {
                if let Some(watcher) = owner.watcher.as_mut() {
                    if let Err(error) = watcher.retarget(&owner.options.path) {
                        tracing::warn!(error=%error,"failed to retarget server settings watcher");
                    }
                }
                owner.cached = None;
                match owner.load().cloned() {
                    Ok(settings) => owner.publish(&settings),
                    Err(error) => tracing::warn!(error=%error,"failed to reload server settings"),
                }
            }
            Command::Shutdown => break,
        }
    }
}

/// The original loader removes comments and trailing commas while protecting
/// quoted JSON strings, and migrates the old streaming mode before decoding.
pub fn decode_settings(bytes: &[u8]) -> Result<ServerSettings, serde_json::Error> {
    let raw = String::from_utf8_lossy(bytes);
    static COMMENTS: std::sync::OnceLock<(regex::Regex, regex::Regex, regex::Regex)> =
        std::sync::OnceLock::new();
    let (line,block,comma)=COMMENTS.get_or_init(|| (
        regex::Regex::new(r#"("(?:[^"\\]|\\.)*")|//[^\n]*"#).unwrap(),
        regex::Regex::new(r#"("(?:[^"\\]|\\.)*")|/\*(?s:.*?)\*/"#).unwrap(),
        regex::Regex::new(r#"("(?:[^"\\]|\\.)*")|,([\x09-\x0D\x20\u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]*[}\]])"#).unwrap(),
    ));
    let stripped = line.replace_all(&raw, |captures: &regex::Captures<'_>| {
        captures
            .get(1)
            .map_or("", |quoted| quoted.as_str())
            .to_owned()
    });
    let stripped = block.replace_all(&stripped, |captures: &regex::Captures<'_>| {
        captures
            .get(1)
            .map_or("", |quoted| quoted.as_str())
            .to_owned()
    });
    let normalized = comma.replace_all(&stripped, |captures: &regex::Captures<'_>| {
        captures
            .get(1)
            .or_else(|| captures.get(2))
            .unwrap()
            .as_str()
            .to_owned()
    });
    let mut value: Value = serde_json::from_str(&normalized)?;
    // The persisted schema overrides these fields with ordinary defaults and
    // optionalKey, rather than the wire codec's null-to-undefined semantics.
    if value
        .get("responseStreamingMode")
        .is_some_and(Value::is_null)
        || value
            .get("projectSettingsOverrides")
            .is_some_and(Value::is_null)
        || value
            .get("projectSettingsOverrides")
            .and_then(Value::as_object)
            .is_some_and(|entries| {
                entries.values().any(|project| {
                    project
                        .get("responseStreamingMode")
                        .is_some_and(Value::is_null)
                })
            })
    {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "Invalid persisted streaming settings",
        ));
    }
    if value["responseStreamingMode"] == "token" {
        value["responseStreamingMode"] = json!("paragraph");
    }
    if let Some(overrides) = value
        .get_mut("projectSettingsOverrides")
        .and_then(Value::as_object_mut)
    {
        for project in overrides.values_mut() {
            if project["responseStreamingMode"] == "token" {
                project["responseStreamingMode"] = json!("paragraph");
            }
        }
    }
    serde_json::from_value(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_settings_secrets::{REDACTED, provider_secret_name};
    use std::sync::{Condvar, Mutex as StdMutex};

    struct HeldRead {
        entered: StdMutex<Option<oneshot::Sender<()>>>,
        release: StdMutex<bool>,
        wake: Condvar,
        fail: bool,
    }
    impl SecretBackend for HeldRead {
        fn get(
            &self,
            _: &str,
        ) -> Result<Option<Vec<u8>>, crate::server_secret_store::SecretStoreError> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
                let mut released = self.release.lock().unwrap();
                while !*released {
                    released = self.wake.wait(released).unwrap();
                }
            }
            if self.fail {
                Err(crate::server_secret_store::SecretStoreError {
                    operation: crate::server_secret_store::Operation::Read,
                    resource: "fixture secret".into(),
                    cause: None,
                })
            } else {
                Ok(Some(b"materialized fixture".to_vec()))
            }
        }
        fn set(
            &self,
            _: &str,
            _: &[u8],
        ) -> Result<(), crate::server_secret_store::SecretStoreError> {
            unreachable!()
        }
        fn remove(&self, _: &str) -> Result<(), crate::server_secret_store::SecretStoreError> {
            unreachable!()
        }
    }
    struct ReleaseRead(Arc<HeldRead>);
    impl Drop for ReleaseRead {
        fn drop(&mut self) {
            *self.0.release.lock().unwrap() = true;
            self.0.wake.notify_all();
        }
    }
    #[tokio::test]
    async fn interrupted_receive_retains_materialization_and_normalizes_secret_error_fallback() {
        for fail in [false, true] {
            let (entered, started) = oneshot::channel();
            let secrets = Arc::new(HeldRead {
                entered: StdMutex::new(Some(entered)),
                release: StdMutex::new(false),
                wake: Condvar::new(),
                fail,
            });
            let release = ReleaseRead(secrets.clone());
            let (sender, updates) = mpsc::unbounded_channel();
            let persisted: ServerSettings = serde_json::from_value(json!({
                "providerInstances":{"codex":{"driver":"codex","enabled":false},"fixture":{"driver":"codex","enabled":false,"environment":[{"name":"KEY","value":"","sensitive":true,"valueRedacted":true}]}},
                "textGenerationModelSelection":{"instanceId":"codex","model":"disabled-model"}
            })).unwrap();
            let expected =
                serde_json::to_value(resolve_text_generation(persisted.clone())).unwrap();
            let mut subscription = SettingsSubscription {
                snapshot: ServerSettings::default(),
                updates,
                secrets,
                pending: None,
            };
            sender.send(persisted).unwrap();
            // Exactly the config merge interleaving: the secret read began,
            // then another source wins and drops the receive future.
            tokio::select! {
                value=subscription.recv()=>panic!("held read completed: {}",value.is_some()),
                result=started=>result.unwrap(),
            }
            assert!(subscription.pending.is_some());
            drop(release);
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(3), subscription.recv())
                    .await
                    .unwrap()
                    .unwrap();
            let output = serde_json::to_value(output).unwrap();
            assert_eq!(
                output["textGenerationModelSelection"],
                expected["textGenerationModelSelection"]
            );
            assert_eq!(
                output["textGenerationModelSelection"]["instanceId"],
                "claudeAgent"
            );
            assert_eq!(
                output["textGenerationModelSelection"]["model"],
                "claude-haiku-4-5"
            );
            assert_eq!(
                output["providerInstances"]["fixture"]["environment"][0]["value"],
                if fail { "" } else { "materialized fixture" }
            );
            drop(sender);
            assert!(
                subscription.recv().await.is_none(),
                "dequeued change was delivered twice"
            );
        }
    }

    struct GateWriter {
        entered: StdMutex<Option<oneshot::Sender<()>>>,
        release: StdMutex<bool>,
        wake: Condvar,
        fail: AtomicBool,
    }
    impl SettingsWriter for GateWriter {
        fn write(&self, path: &Path, contents: &str) -> std::io::Result<()> {
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(std::io::Error::other("injected pre-rename failure"));
            }
            AtomicSettingsWriter.write(path, contents)?;
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
                let mut release = self.release.lock().unwrap();
                while !*release {
                    release = self.wake.wait(release).unwrap();
                }
            }
            Ok(())
        }
    }
    struct Release(Arc<GateWriter>);
    impl Drop for Release {
        fn drop(&mut self) {
            *self.0.release.lock().unwrap() = true;
            self.0.wake.notify_all();
        }
    }
    fn patch(secret: &str, enabled: bool) -> ServerSettingsPatch {
        serde_json::from_value(json!({"providerInstances":{"fixture":{"driver":"codex","environment":[{"name":"KEY","value":secret,"sensitive":true}]}},"providers":{"codex":{"enabled":enabled}}})).unwrap()
    }
    #[tokio::test]
    async fn cancelled_caller_after_rename_cannot_rollback_secrets_or_skip_publication() {
        let directory = tempfile::tempdir().unwrap();
        let secrets = ServerSecretStore::open(directory.path().join("secrets")).unwrap();
        let path = directory.path().join("settings.json");
        let (entered, landed) = oneshot::channel();
        let writer = Arc::new(GateWriter {
            entered: StdMutex::new(Some(entered)),
            release: StdMutex::new(false),
            wake: Condvar::new(),
            fail: AtomicBool::new(false),
        });
        let release = Release(writer.clone());
        let service = SettingsService::start(SettingsOptions {
            path: path.clone(),
            secrets: Arc::new(secrets.clone()),
            writer,
            watch: false,
        })
        .await
        .unwrap();
        let mut changes = service.subscribe().await.unwrap();
        let caller = tokio::spawn({
            let service = service.clone();
            async move { service.update(patch("committed secret", false)).await }
        });
        landed.await.unwrap(); // The actual atomic rename has already succeeded.
        assert_eq!(
            secrets
                .get(&provider_secret_name("fixture", "KEY"))
                .unwrap(),
            Some(b"committed secret".to_vec())
        );
        let persisted: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted["providerInstances"]["fixture"]["environment"][0]["value"],
            ""
        );
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        drop(release);
        let published = changes.recv().await.unwrap();
        assert_eq!(
            serde_json::to_value(&published).unwrap()["providerInstances"]["fixture"]["environment"]
                [0]["value"],
            "committed secret"
        );
        assert_eq!(
            serde_json::to_value(service.snapshot().await.unwrap()).unwrap(),
            serde_json::to_value(published).unwrap()
        );
        service.shutdown().await;
        let reopened = SettingsService::start(SettingsOptions::file(path, secrets))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(reopened.snapshot().await.unwrap()).unwrap()["providerInstances"]
                ["fixture"]["environment"][0]["value"],
            "committed secret"
        );
        reopened.shutdown().await;
    }
    #[tokio::test]
    async fn failed_disk_write_restores_secret_file_cache_and_change_stream() {
        let directory = tempfile::tempdir().unwrap();
        let secrets = ServerSecretStore::open(directory.path().join("secrets")).unwrap();
        let path = directory.path().join("settings.json");
        let writer = Arc::new(GateWriter {
            entered: StdMutex::new(None),
            release: StdMutex::new(true),
            wake: Condvar::new(),
            fail: AtomicBool::new(false),
        });
        let service = SettingsService::start(SettingsOptions {
            path: path.clone(),
            secrets: Arc::new(secrets.clone()),
            writer: writer.clone(),
            watch: false,
        })
        .await
        .unwrap();
        service
            .update(patch("previous secret", true))
            .await
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let mut changes = service.subscribe().await.unwrap();
        writer.fail.store(true, Ordering::SeqCst);
        assert_eq!(
            service
                .update(patch("new secret", false))
                .await
                .unwrap_err()
                .operation,
            "write-file"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            secrets
                .get(&provider_secret_name("fixture", "KEY"))
                .unwrap(),
            Some(b"previous secret".to_vec())
        );
        assert_eq!(
            serde_json::to_value(service.snapshot().await.unwrap()).unwrap(),
            serde_json::to_value(&changes.snapshot).unwrap()
        );
        assert!(changes.updates.try_recv().is_err());
        service.update(patch(REDACTED, false)).await.unwrap();
        assert!(changes.recv().await.is_some());
        service.shutdown().await;
        assert!(changes.recv().await.is_none());
    }
    #[tokio::test]
    async fn jsonc_reload_preserves_untrusted_file_and_subscription_has_atomic_seed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path,b"{/* retained comment */\"responseStreamingMode\":\"token\",\"projectSettingsOverrides\":{\"fixture\":{\"responseStreamingMode\":\"token\",},},}").unwrap();
        let secrets = ServerSecretStore::open(directory.path().join("secrets")).unwrap();
        let service = SettingsService::start(SettingsOptions::file(path.clone(), secrets))
            .await
            .unwrap();
        let snapshot = serde_json::to_value(service.snapshot().await.unwrap()).unwrap();
        assert_eq!(snapshot["responseStreamingMode"], "paragraph");
        assert_eq!(
            snapshot["projectSettingsOverrides"]["fixture"]["responseStreamingMode"],
            "paragraph"
        );
        let mut subscription = service.subscribe().await.unwrap();
        let invalid = b"{invalid,do-not-overwrite}";
        std::fs::write(&path, invalid).unwrap();
        let defaults = service.reload().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
        assert_eq!(
            serde_json::to_value(subscription.recv().await.unwrap()).unwrap(),
            serde_json::to_value(defaults).unwrap()
        );
        std::fs::write(&path, b"{\"providers\":{\"codex\":{\"enabled\":false}}}").unwrap();
        service.reload().await.unwrap();
        assert_eq!(
            serde_json::to_value(subscription.recv().await.unwrap()).unwrap()["providers"]["codex"]
                ["enabled"],
            false
        );
        service.shutdown().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn actual_watcher_observes_target_edits_retargeting_and_dangling_target_creation() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let first = root.join("first/settings.json");
        let second = root.join("second/settings.json");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        let contents = |label: &str| {
            serde_json::to_vec(&json!({"providers":{"codex":{"binaryPath":label}}})).unwrap()
        };
        std::fs::write(&first, contents("first")).unwrap();
        std::fs::write(&second, contents("second")).unwrap();
        let path = root.join("settings.json");
        symlink(&first, &path).unwrap();
        let secrets = ServerSecretStore::open(root.join("secrets")).unwrap();
        let service = SettingsService::start(SettingsOptions::file(path.clone(), secrets))
            .await
            .unwrap();
        let mut subscription = service.subscribe().await.unwrap();
        async fn observed(subscription: &mut SettingsSubscription, expected: &str) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let snapshot = subscription.recv().await.unwrap();
                    if serde_json::to_value(snapshot).unwrap()["providers"]["codex"]["binaryPath"]
                        == expected
                    {
                        break;
                    }
                }
            })
            .await
            .expect("filesystem update should reach settings subscribers");
        }
        std::fs::write(&first, contents("edited-first")).unwrap();
        observed(&mut subscription, "edited-first").await;
        std::fs::remove_file(&path).unwrap();
        symlink(&second, &path).unwrap();
        observed(&mut subscription, "second").await;
        std::fs::write(&second, contents("edited-second")).unwrap();
        observed(&mut subscription, "edited-second").await;
        let dangling = root.join("new/target/settings.json");
        std::fs::remove_file(&path).unwrap();
        symlink(&dangling, &path).unwrap();
        // Receiving the default snapshot proves watcher retargeting ran and
        // created the missing parent before the formerly dangling target is written.
        observed(&mut subscription, "codex").await;
        assert!(dangling.parent().unwrap().is_dir());
        std::fs::write(&dangling, contents("created-target")).unwrap();
        observed(&mut subscription, "created-target").await;
        service.shutdown().await;
        assert!(subscription.recv().await.is_none());
    }
}
