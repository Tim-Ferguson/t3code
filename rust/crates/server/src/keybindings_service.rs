//! The keybindings owner (port of the `Keybindings` service in `apps/server/src/keybindings.ts`),
//! structured like `server_settings.rs`: one thread owns the file, the cached resolved state and
//! the subscribers, so reads, writes and file-change reloads are serialized and a write finishes
//! even if its caller goes away.
//!
//! The thread is a plain OS thread, not a blocking-pool task: services here are shared per file
//! path for the life of the process (see `shared`), and a pool task would keep a short-lived
//! runtime from shutting down.
use crate::{
    keybindings::{
        MAX_KEYBINDINGS_COUNT, Resolved, Rule, compile_config, decode_entry, default_rules,
        has_same_shortcut_context, is_same_rule, merge_with_defaults,
    },
    server_secret_store::write_string_atomically,
    server_settings::SettingsWatch,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, mpsc as std_mpsc},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

const DEBOUNCE: Duration = Duration::from_millis(100);

/// Default rules added to a command after startup sync had already persisted that command.
/// Backfill skips commands a config already has, so startup adds each rule once per config, only
/// next to the untouched earlier default (`alongside`), and records its id. Customized commands
/// and later removals stay as the user set them.
struct LateDefault {
    id: &'static str,
    alongside: (&'static str, &'static str, &'static str),
    rule: (&'static str, &'static str, &'static str),
}
const LATE_DEFAULTS: [LateDefault; 1] = [LateDefault {
    id: "composer.sendBackground:mod+enter",
    alongside: ("mod+alt+enter", "composer.sendBackground", "composerFocus && draftThreadRoute"),
    rule: ("mod+enter", "composer.sendBackground", "composerFocus && draftThreadRoute"),
}];

fn late_rule((key, command, when): (&str, &str, &str)) -> Rule {
    Rule { key: key.into(), command: command.into(), when: Some(when.into()) }
}

// ---- errors ----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Cause {
    /// A schema failure: the text the original's `SchemaError` carries.
    Schema(String),
    /// A filesystem failure, in the shape of an Effect `PlatformError`.
    Io { operation: &'static str, path: PathBuf, kind: std::io::ErrorKind, message: String },
    /// The original wraps a read failure of the writable load in its own parse error.
    Nested(Box<KeybindingsError>),
    Stopped,
}

/// `KeybindingsConfigError`, which the original tags `KeybindingsConfigParseError`.
#[derive(Debug, Clone)]
pub struct KeybindingsError {
    pub config_path: PathBuf,
    pub detail: &'static str,
    pub cause: Cause,
}

impl KeybindingsError {
    fn io(path: &Path, detail: &'static str, operation: &'static str, error: std::io::Error) -> Self {
        Self {
            config_path: path.to_path_buf(),
            detail,
            cause: Cause::Io { operation, path: path.to_path_buf(), kind: error.kind(), message: error.to_string() },
        }
    }
    pub fn message(&self) -> String {
        format!(
            "Unable to parse keybindings config at {}: {}",
            self.config_path.display(),
            self.detail
        )
    }
    fn cause_json(&self) -> Value {
        match &self.cause {
            Cause::Schema(message) => json!({"name":"SchemaError","message":message}),
            Cause::Io { operation, path, kind, message } => {
                let reason = match kind {
                    std::io::ErrorKind::PermissionDenied => "PermissionDenied",
                    std::io::ErrorKind::NotFound => "NotFound",
                    std::io::ErrorKind::AlreadyExists => "AlreadyExists",
                    _ if message.contains("directory") => "BadResource",
                    _ => "Unknown",
                };
                json!({"name":"PlatformError","message":format!("{reason}: {operation} ({})", path.display()),"cause":{"name":"Error","message":message}})
            }
            Cause::Nested(inner) => {
                json!({"name":"KeybindingsConfigParseError","message":inner.message(),"cause":inner.cause_json()})
            }
            Cause::Stopped => json!({"name":"Error","message":"The keybindings service has stopped."}),
        }
    }
    /// The `_tag` envelope clients decode (`KeybindingsConfigError` in contracts).
    pub fn wire(&self) -> Value {
        json!({"_tag":"KeybindingsConfigParseError","configPath":self.config_path,"detail":self.detail,"cause":self.cause_json()})
    }
    fn stopped(path: &Path) -> Self {
        Self { config_path: path.to_path_buf(), detail: "keybindings service stopped", cause: Cause::Stopped }
    }
}

// ---- state -----------------------------------------------------------------------------------

/// The resolved bindings and the non-fatal problems found loading them.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigState {
    pub keybindings: Vec<Resolved>,
    pub issues: Vec<Value>,
}

impl ConfigState {
    pub fn keybindings_json(&self) -> Value {
        Value::Array(self.keybindings.iter().map(Resolved::to_value).collect())
    }
}

fn issue_message(message: &str) -> String {
    let trimmed = t3_contracts::trim_wire_string(message);
    if trimmed.is_empty() { "Invalid keybindings configuration.".into() } else { trimmed.into() }
}
fn malformed_issue(detail: &str) -> Value {
    json!({"kind":"keybindings.malformed-config","message":issue_message(detail)})
}
fn invalid_entry_issue(index: usize, detail: &str) -> Value {
    json!({"kind":"keybindings.invalid-entry","index":index,"message":issue_message(detail)})
}

// ---- the service handle ----------------------------------------------------------------------

type Reply<T> = oneshot::Sender<Result<T, KeybindingsError>>;

enum Command {
    State(Reply<ConfigState>),
    Upsert(Rule, Option<Rule>, Reply<Vec<Resolved>>),
    Remove(Rule, Reply<Vec<Resolved>>),
    Subscribe(Reply<(ConfigState, mpsc::UnboundedReceiver<ConfigState>)>),
    FileEvent,
    Ping,
    Shutdown,
}

struct Inner {
    path: PathBuf,
    sender: std_mpsc::Sender<Command>,
    /// Set when the owner thread has ended.
    exited: tokio::sync::watch::Receiver<bool>,
}
/// Marks the owner thread's end, however it ends.
struct Exited(tokio::sync::watch::Sender<bool>);
impl Drop for Exited {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.sender.send(Command::Shutdown);
    }
}

#[derive(Clone)]
pub struct KeybindingsService {
    inner: Arc<Inner>,
}

/// Rules as the socket handlers hand them over: already trimmed by the payload decode.
pub struct UpsertInput {
    pub rule: Rule,
    pub replace: Option<Rule>,
}

impl KeybindingsService {
    /// Starts the owner: creates the config directory, attaches the watcher (when `watch`), runs
    /// the startup default sync, and loads the first state. A startup failure is logged and the
    /// service keeps running, so later reads retry, as the original's `start` does.
    pub async fn start(path: PathBuf, watch: bool) -> Self {
        let (sender, commands) = std_mpsc::channel();
        let (ready, started) = oneshot::channel();
        let thread_path = path.clone();
        let events = sender.clone();
        let (exit, exited) = tokio::sync::watch::channel(false);
        std::thread::Builder::new()
            .name("keybindings".into())
            .spawn(move || {
                let _exited = Exited(exit);
                run(thread_path, watch, commands, events, ready)
            })
            .expect("spawn keybindings thread");
        if let Ok(Err(error)) = started.await {
            tracing::warn!(path=%path.display(),detail=error.detail,"failed to start keybindings runtime");
        }
        Self { inner: Arc::new(Inner { path, sender, exited }) }
    }

    /// Ends the owner thread and returns once it has. Later requests report the service stopped.
    pub async fn shutdown(&self) {
        let _ = self.inner.sender.send(Command::Shutdown);
        let mut exited = self.inner.exited.clone();
        let _ = exited.wait_for(|exited| *exited).await;
    }

    /// Resolves when the owner thread has ended, which dropping the last handle also causes.
    pub fn exit_signal(&self) -> tokio::sync::watch::Receiver<bool> {
        self.inner.exited.clone()
    }

    /// Ends every shared service and forgets them, so the server's shutdown leaves no owner thread
    /// behind. A service asked for after this starts afresh.
    pub async fn shutdown_shared() {
        let services: Vec<KeybindingsService> = shared_registry().lock().await.drain().map(|(_, service)| service).collect();
        for service in services {
            service.shutdown().await;
        }
    }

    /// The process-wide service for one config file, started on first use. Production starts it
    /// with the server configuration; socket handlers find it by the path the configuration
    /// carries, so the file has one owner however many connections ask.
    pub async fn shared(path: &Path) -> Self {
        let mut services = shared_registry().lock().await;
        if let Some(service) = services.get(path) {
            if service.is_running() {
                return service.clone();
            }
        }
        let service = Self::start(path.to_path_buf(), true).await;
        services.insert(path.to_path_buf(), service.clone());
        service
    }

    fn is_running(&self) -> bool {
        // A send fails once the owner thread has exited.
        self.inner.sender.send(Command::Ping).is_ok()
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(Reply<T>) -> Command,
    ) -> Result<T, KeybindingsError> {
        let (reply, receive) = oneshot::channel();
        let stopped = || KeybindingsError::stopped(&self.inner.path);
        self.inner.sender.send(command(reply)).map_err(|_| stopped())?;
        receive.await.map_err(|_| stopped())?
    }

    /// The resolved bindings and issues, from cache or disk.
    pub async fn state(&self) -> Result<ConfigState, KeybindingsError> {
        self.request(Command::State).await
    }
    pub async fn upsert(&self, input: UpsertInput) -> Result<Vec<Resolved>, KeybindingsError> {
        self.request(|reply| Command::Upsert(input.rule, input.replace, reply)).await
    }
    pub async fn remove(&self, rule: Rule) -> Result<Vec<Resolved>, KeybindingsError> {
        self.request(|reply| Command::Remove(rule, reply)).await
    }
    /// The current state and every later change; no change falls between the two.
    pub async fn subscribe(&self) -> Result<Subscription, KeybindingsError> {
        let (snapshot, updates) = self.request(Command::Subscribe).await?;
        Ok(Subscription { snapshot, updates })
    }
}

fn shared_registry() -> &'static tokio::sync::Mutex<HashMap<PathBuf, KeybindingsService>> {
    static REGISTRY: OnceLock<tokio::sync::Mutex<HashMap<PathBuf, KeybindingsService>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub struct Subscription {
    pub snapshot: ConfigState,
    updates: mpsc::UnboundedReceiver<ConfigState>,
}
impl Subscription {
    /// The next published state; None once the service has stopped.
    pub async fn recv(&mut self) -> Option<ConfigState> {
        self.updates.recv().await
    }
}

// ---- the owner -------------------------------------------------------------------------------

struct Owner {
    path: PathBuf,
    cached: Option<ConfigState>,
    listeners: Vec<mpsc::UnboundedSender<ConfigState>>,
    watcher: Option<SettingsWatch>,
}

fn run(
    path: PathBuf,
    watch: bool,
    commands: std_mpsc::Receiver<Command>,
    events: std_mpsc::Sender<Command>,
    ready: oneshot::Sender<Result<(), KeybindingsError>>,
) {
    let mut owner = Owner { path, cached: None, listeners: Vec::new(), watcher: None };
    let startup = owner.startup(watch, events);
    let _ = ready.send(startup);
    let mut dirty: Option<Instant> = None;
    loop {
        let command = match dirty {
            None => commands.recv().map_err(|_| std_mpsc::RecvTimeoutError::Disconnected),
            Some(deadline) => commands.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        };
        match command {
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                dirty = None;
                owner.revalidate_and_emit();
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) | Ok(Command::Shutdown) => break,
            // Editors emit several events per save; the file is read once they go quiet.
            Ok(Command::FileEvent) => dirty = Some(Instant::now() + DEBOUNCE),
            Ok(Command::Ping) => {}
            Ok(Command::State(reply)) => {
                if !reply.is_closed() {
                    let _ = reply.send(owner.load_state());
                }
            }
            // A write started by a caller that has since gone away still finishes.
            Ok(Command::Upsert(rule, replace, reply)) => {
                let _ = reply.send(owner.upsert(rule, replace));
            }
            Ok(Command::Remove(rule, reply)) => {
                let _ = reply.send(owner.remove(rule));
            }
            Ok(Command::Subscribe(reply)) => {
                if !reply.is_closed() {
                    let result = owner.load_state().map(|snapshot| {
                        let (sender, updates) = mpsc::unbounded_channel();
                        owner.listeners.push(sender);
                        (snapshot, updates)
                    });
                    let _ = reply.send(result);
                }
            }
        }
    }
}

impl Owner {
    fn startup(
        &mut self,
        watch: bool,
        events: std_mpsc::Sender<Command>,
    ) -> Result<(), KeybindingsError> {
        let directory = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(directory).map_err(|error| {
            KeybindingsError::io(&self.path, "failed to prepare keybindings config directory", "FileSystem.makeDirectory", error)
        })?;
        if watch {
            let (notify, mut observed) = mpsc::unbounded_channel();
            let prepared = SettingsWatch::new(notify).map_err(std::io::Error::other).and_then(|mut watcher| {
                watcher.retarget(&self.path).map_err(|error| std::io::Error::other(error.to_string()))?;
                Ok(watcher)
            });
            match prepared {
                Ok(watcher) => {
                    self.watcher = Some(watcher);
                    // Forwards watcher events into the command queue. Ends when the watcher drops.
                    std::thread::spawn(move || {
                        while observed.blocking_recv().is_some() {
                            if events.send(Command::FileEvent).is_err() {
                                return;
                            }
                        }
                    });
                }
                Err(error) => tracing::warn!(error=%error,"failed to watch the keybindings file"),
            }
        }
        self.sync_defaults()?;
        self.cached = None;
        self.load_state().map(|_| ())
    }

    // -- reading --

    fn exists(&self) -> Result<bool, KeybindingsError> {
        self.path.try_exists().map_err(|error| {
            KeybindingsError::io(&self.path, "failed to access keybindings config", "FileSystem.exists", error)
        })
    }

    fn read_text(&self) -> Result<String, KeybindingsError> {
        let bytes = std::fs::read(&self.path).map_err(|error| {
            KeybindingsError::io(&self.path, "failed to read keybindings config", "FileSystem.readFile", error)
        })?;
        // The original decodes the file with a text decoder, which drops a leading byte order mark.
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// The file as a lenient-JSON array (comments and trailing commas allowed). Err is the
    /// schema failure text.
    fn decode_entries(text: &str) -> Result<Vec<Value>, &'static str> {
        match crate::server_settings::decode_jsonc(text.as_bytes()) {
            Err(_) => Err("Expected a valid JSON string"),
            Ok(Value::Array(entries)) => Ok(entries),
            Ok(_) => Err("Expected array"),
        }
    }

    /// For writes: every entry that decodes, or the refusal to touch a malformed file.
    fn load_writable(&self) -> Result<Vec<Rule>, KeybindingsError> {
        if !self.exists()? {
            return Ok(Vec::new());
        }
        let refused = |cause: Cause| KeybindingsError {
            config_path: self.path.clone(),
            detail: "expected JSON array",
            cause,
        };
        let text = self.read_text().map_err(|error| refused(Cause::Nested(Box::new(error))))?;
        let entries = Self::decode_entries(&text)
            .map_err(|message| refused(Cause::Schema(message.into())))?;
        Ok(entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match decode_entry(entry) {
                Ok(rule) => Some(rule),
                Err(detail) => {
                    tracing::warn!(path=%self.path.display(),index,error=%detail,"ignoring invalid keybinding entry");
                    None
                }
            })
            .collect())
    }

    /// For the runtime: the rules that decode, and an issue for each thing wrong with the file.
    fn load_runtime(&self) -> Result<(Vec<Rule>, Vec<Value>), KeybindingsError> {
        if !self.exists()? {
            return Ok((Vec::new(), Vec::new()));
        }
        let text = self.read_text()?;
        let entries = match Self::decode_entries(&text) {
            Ok(entries) => entries,
            Err(message) => {
                let detail = format!("expected JSON array (SchemaError: {message})");
                return Ok((Vec::new(), vec![malformed_issue(&detail)]));
            }
        };
        let (mut rules, mut issues) = (Vec::new(), Vec::new());
        for (index, entry) in entries.iter().enumerate() {
            match decode_entry(entry) {
                Ok(rule) => rules.push(rule),
                Err(detail) => {
                    tracing::warn!(path=%self.path.display(),index,error=%detail,"ignoring invalid keybinding entry");
                    issues.push(invalid_entry_issue(index, &detail));
                }
            }
        }
        Ok((rules, issues))
    }

    /// Cached after a good read; a failed read is not kept, so the next one retries.
    fn load_state(&mut self) -> Result<ConfigState, KeybindingsError> {
        if let Some(state) = &self.cached {
            return Ok(state.clone());
        }
        let (rules, issues) = self.load_runtime()?;
        let state = ConfigState { keybindings: merge_with_defaults(compile_config(&rules)), issues };
        self.cached = Some(state.clone());
        Ok(state)
    }

    // -- writing --

    fn write(&self, rules: &[Rule]) -> Result<(), KeybindingsError> {
        // Encoding trims the key and `when`, as the original's schema does.
        let trimmed: Vec<Rule> = rules
            .iter()
            .map(|rule| Rule {
                key: t3_contracts::trim_wire_string(&rule.key).to_owned(),
                command: rule.command.clone(),
                when: rule.when.as_deref().map(|when| t3_contracts::trim_wire_string(when).to_owned()),
            })
            .collect();
        let mut contents = serde_json::to_string_pretty(&trimmed).expect("rules encode");
        contents.push('\n');
        write_string_atomically(&self.path, &contents).map_err(|error| {
            // The original names the temporary directory prefix it failed to make: `<file>.`.
            let mut error = KeybindingsError::io(&self.path, "failed to write keybindings config", "FileSystem.makeTempDirectory", error);
            if let Cause::Io { path, .. } = &mut error.cause {
                path.as_mut_os_string().push(".");
            }
            error
        })
    }

    fn publish(&mut self, state: &ConfigState) {
        self.listeners.retain(|listener| listener.send(state.clone()).is_ok());
    }

    /// Caches and publishes what a write just made current.
    fn commit(&mut self, rules: &[Rule]) -> Vec<Resolved> {
        let state = ConfigState { keybindings: merge_with_defaults(compile_config(rules)), issues: Vec::new() };
        self.cached = Some(state.clone());
        self.publish(&state);
        state.keybindings
    }

    fn upsert(&mut self, rule: Rule, replace: Option<Rule>) -> Result<Vec<Resolved>, KeybindingsError> {
        let custom = self.load_writable()?;
        let mut next: Vec<Rule> = custom
            .into_iter()
            .filter(|entry| match &replace {
                Some(target) => !is_same_rule(entry, target) && !is_same_rule(entry, &rule),
                None => !is_same_rule(entry, &rule),
            })
            .collect();
        next.push(rule);
        if next.len() > MAX_KEYBINDINGS_COUNT {
            tracing::warn!(path=%self.path.display(),max=MAX_KEYBINDINGS_COUNT,"truncating keybindings config to max entries");
            next.drain(..next.len() - MAX_KEYBINDINGS_COUNT);
        }
        self.write(&next)?;
        Ok(self.commit(&next))
    }

    fn remove(&mut self, target: Rule) -> Result<Vec<Resolved>, KeybindingsError> {
        let next: Vec<Rule> = self.load_writable()?.into_iter().filter(|entry| !is_same_rule(entry, &target)).collect();
        self.write(&next)?;
        Ok(self.commit(&next))
    }

    fn revalidate_and_emit(&mut self) {
        if let Some(watcher) = self.watcher.as_mut() {
            if let Err(error) = watcher.retarget(&self.path) {
                tracing::warn!(error=%error,"failed to retarget the keybindings watcher");
            }
        }
        let previous = self.cached.take();
        match self.load_state() {
            // A save that leaves the resolved bindings and issues as they were, such as the echo
            // of this service's own write, is not news to subscribers.
            Ok(state) if previous.as_ref() == Some(&state) => {}
            Ok(state) => self.publish(&state),
            Err(error) => tracing::warn!(detail=error.detail,"failed to reload keybindings"),
        }
    }

    // -- startup sync --

    fn migrations_path(&self) -> PathBuf {
        self.path.parent().unwrap_or(Path::new(".")).join("keybindings-migrations")
    }
    fn applied_migrations(&self) -> Vec<String> {
        std::fs::read_to_string(self.migrations_path())
            .map(|text| text.split('\n').filter(|line| !line.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default()
    }
    fn record_migrations(&self, ids: impl IntoIterator<Item = String>) -> Result<(), KeybindingsError> {
        let mut unique: Vec<String> = Vec::new();
        for id in ids {
            if !unique.contains(&id) {
                unique.push(id);
            }
        }
        write_string_atomically(&self.migrations_path(), &format!("{}\n", unique.join("\n"))).map_err(|error| {
            KeybindingsError::io(&self.path, "failed to record keybinding migrations", "FileSystem.makeTempDirectory", error)
        })
    }

    /// Ensures the file exists and holds every default command, so defaults added in a newer
    /// version appear on startup without touching what the user set.
    fn sync_defaults(&mut self) -> Result<(), KeybindingsError> {
        let applied = self.applied_migrations();
        let pending_late: Vec<&LateDefault> =
            LATE_DEFAULTS.iter().filter(|late| !applied.iter().any(|id| id == late.id)).collect();
        let defaults = default_rules();
        if !self.exists()? {
            self.write(defaults)?;
            if !pending_late.is_empty() {
                self.record_migrations(applied.iter().cloned().chain(LATE_DEFAULTS.iter().map(|late| late.id.to_owned())))?;
            }
            self.cached = None;
            return Ok(());
        }
        let (custom, issues) = self.load_runtime()?;
        if !issues.is_empty() {
            tracing::warn!(path=%self.path.display(),issues=?issues,"skipping startup keybindings default sync because config has issues");
            self.cached = None;
            return Ok(());
        }
        let existing: std::collections::HashSet<&str> = custom.iter().map(|entry| entry.command.as_str()).collect();
        // Each missing rule remembers the late default it came from, if any.
        let mut missing: Vec<(Rule, Option<&'static str>)> = Vec::new();
        for default in defaults {
            if existing.contains(default.command.as_str()) {
                continue;
            }
            if let Some(conflict) = custom.iter().find(|entry| has_same_shortcut_context(entry, default)) {
                tracing::warn!(path=%self.path.display(),default_command=%default.command,conflicting_command=%conflict.command,key=%default.key,"skipping default keybinding due to shortcut conflict");
                continue;
            }
            missing.push((default.clone(), None));
        }
        // The loop above backfills whole missing commands. Late defaults join an untouched
        // earlier default, when their shortcut is still free.
        for late in &pending_late {
            let (alongside, rule) = (late_rule(late.alongside), late_rule(late.rule));
            if custom.iter().any(|entry| is_same_rule(entry, &alongside))
                && !custom.iter().any(|entry| has_same_shortcut_context(entry, &rule))
            {
                missing.push((rule, Some(late.id)));
            }
        }
        // Startup backfill never evicts persisted user rules: append only what fits.
        let available = MAX_KEYBINDINGS_COUNT.saturating_sub(custom.len());
        let split = available.min(missing.len());
        let skipped: Vec<(Rule, Option<&'static str>)> = missing.split_off(split);
        if !skipped.is_empty() {
            tracing::warn!(path=%self.path.display(),max=MAX_KEYBINDINGS_COUNT,"skipping default keybinding backfill at max entries");
        }
        if !missing.is_empty() {
            let mut next = custom.clone();
            next.extend(missing.into_iter().map(|(rule, _)| rule));
            self.write(&next)?;
        }
        // A late default skipped at max entries stays pending for a later start.
        let settled: Vec<String> = pending_late
            .iter()
            .filter(|late| !skipped.iter().any(|(_, id)| *id == Some(late.id)))
            .map(|late| late.id.to_owned())
            .collect();
        if !settled.is_empty() {
            self.record_migrations(applied.iter().cloned().chain(settled))?;
        }
        self.cached = None;
        Ok(())
    }
}

#[cfg(test)]
#[path = "keybindings_service_tests.rs"]
mod tests;

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    /// A service's owner thread ends when its last handle is dropped, and `shutdown_shared` ends
    /// the shared services and forgets them, so a later request starts a fresh owner.
    #[tokio::test]
    async fn the_owner_thread_ends_with_its_service_and_with_the_servers_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let dropped = KeybindingsService::start(directory.path().join("dropped.json"), true).await;
        let mut exited = dropped.exit_signal();
        assert!(!*exited.borrow());
        drop(dropped);
        exited.wait_for(|exited| *exited).await.unwrap();

        let path = directory.path().join("keybindings.json");
        let shared = KeybindingsService::shared(&path).await;
        assert!(shared.state().await.is_ok());
        let first = shared.exit_signal();
        KeybindingsService::shutdown_shared().await;
        assert!(*first.borrow(), "shutdown returns once the owner thread has ended");
        assert!(shared.state().await.is_err(), "an ended service reports itself stopped");
        let again = KeybindingsService::shared(&path).await;
        assert!(again.state().await.is_ok(), "a service asked for after shutdown starts afresh");
        let second = again.exit_signal();
        KeybindingsService::shutdown_shared().await;
        assert!(*second.borrow());
    }
}
