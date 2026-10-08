//! Thread-scoped PTY lifecycle, retained histories and atomic snapshot/live streams.
use crate::{
    terminal_environment::{Environment, shell_candidates, spawn_environment},
    terminal_history::{BoundedTerminalHistory, TerminalHistoryFilter},
    terminal_io::{TerminalIo, TerminalProcess},
    terminal_process::{PtyEvent, PtySpawn},
    terminal_store::{TerminalFailure, TerminalHistoryStore, io_cause},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use t3_contracts::*;
use tokio::sync::{mpsc, watch};

pub struct TerminalManagerOptions {
    pub logs_directory: PathBuf,
    pub environment: Environment,
    pub home_directory: String,
    pub platform: String,
    pub shell: Option<String>,
    pub history_lines: usize,
    pub history_bytes: usize,
    pub kill_grace: Duration,
    pub retained_inactive: usize,
    pub provider_instances: Option<ProviderInstanceConfigMap>,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    async fn fixture() -> (tempfile::TempDir, TerminalManager) {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("terminal-fixture.py");
        std::fs::write(&script,"#!/usr/bin/env python3\nimport os,sys,tty\ntty.setraw(0)\nos.write(1,b'READY\\n')\nfor line in sys.stdin:\n line=line.rstrip('\\n')\n if line=='QUIT':\n  os.write(1,b'FINAL\\n');sys.exit(7)\n elif line=='ENV': os.write(1,('ENV='+os.environ.get('VALUE','')+'\\n').encode())\n else: os.write(1,(line+'\\n').encode())\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut options =
            TerminalManagerOptions::host(directory.path().join("logs"), &ServerSettings::default());
        options.shell = Some(script.to_string_lossy().into_owned());
        options.kill_grace = Duration::ZERO;
        let manager = TerminalManager::new(options).await.unwrap();
        (directory, manager)
    }
    fn open_input(directory: &std::path::Path) -> TerminalOpenInput {
        serde_json::from_value(
            json!({"threadId":"thread-fixture","terminalId":"term-1","cwd":directory}),
        )
        .unwrap()
    }
    fn observe_input() -> TerminalObserveInput {
        serde_json::from_value(json!({"threadId":"thread-fixture","terminalId":"term-1"})).unwrap()
    }
    fn write_input(data: &str) -> TerminalWriteInput {
        serde_json::from_value(
            json!({"threadId":"thread-fixture","terminalId":"term-1","data":data}),
        )
        .unwrap()
    }
    async fn next(subscription: &mut TerminalSubscription) -> Value {
        tokio::time::timeout(Duration::from_secs(3), subscription.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }
    async fn ready(subscription: &mut TerminalSubscription) {
        let mut output = String::new();
        while !output.contains("READY\n") {
            let item = next(subscription).await;
            let _: TerminalAttachStreamEvent = serde_json::from_value(item.clone()).unwrap();
            if item["type"] == "snapshot" {
                output.push_str(item["snapshot"]["history"].as_str().unwrap());
            } else if item["type"] == "output" {
                output.push_str(item["data"].as_str().unwrap());
            } else {
                panic!("{item}");
            }
        }
    }
    #[tokio::test]
    async fn real_pty_live_output_history_filter_exit_order_and_reopen_are_source_compatible() {
        let (directory, manager) = fixture().await;
        let mut metadata = manager.metadata();
        assert_eq!(
            next(&mut metadata).await,
            json!({"type":"snapshot","terminals":[]})
        );
        let opened = manager.open(open_input(directory.path())).await.unwrap();
        assert_eq!(opened.status, TerminalSessionStatus::Running);
        assert_eq!(opened.label.0, "Terminal 1");
        let upsert = next(&mut metadata).await;
        let _: TerminalMetadataStreamEvent = serde_json::from_value(upsert.clone()).unwrap();
        assert_eq!(upsert["type"], "upsert");
        let mut stream = manager.observe(observe_input()).await.unwrap();
        ready(&mut stream).await;
        manager.write(write_input("前😀\u{1b}[6n\n")).await.unwrap();
        let live = next(&mut stream).await;
        assert_eq!(live["type"], "output");
        assert!(live["data"].as_str().unwrap().contains("\u{1b}[6n"));
        manager.write(write_input("QUIT\n")).await.unwrap();
        let final_output = next(&mut stream).await;
        assert_eq!(final_output["type"], "output");
        assert_eq!(final_output["data"], "FINAL\n");
        let exited = next(&mut stream).await;
        assert_eq!(exited["type"], "exited");
        assert_eq!(exited["exitCode"], 7);
        assert!(final_output["sequence"].as_u64().unwrap() < exited["sequence"].as_u64().unwrap());
        // Exited writes are deliberately ignored by the original manager.
        manager.write(write_input("after-exit\n")).await.unwrap();
        let session = manager.session("thread-fixture", "term-1").unwrap();
        manager.stop(&session).await;
        let mut retained = manager
            .0
            .history
            .read("thread-fixture", "term-1")
            .await
            .unwrap();
        assert!(retained.value().contains("前😀"));
        assert!(!retained.value().contains("\u{1b}[6n"));
        manager
            .close(
                serde_json::from_value(json!({"threadId":"thread-fixture","terminalId":"term-1"}))
                    .unwrap(),
            )
            .await
            .unwrap();
        let reopened = manager.open(open_input(directory.path())).await.unwrap();
        assert!(reopened.history.contains("FINAL"));
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn snapshot_is_queued_before_output_while_receiver_is_held() {
        let (directory, manager) = fixture().await;
        manager.open(open_input(directory.path())).await.unwrap();
        let mut stream = manager.observe(observe_input()).await.unwrap();
        manager
            .write(write_input("after-snapshot\n"))
            .await
            .unwrap();
        let first = next(&mut stream).await;
        assert_eq!(first["type"], "snapshot");
        let mut sequence = first["snapshot"]["sequence"].as_u64().unwrap();
        loop {
            let output = next(&mut stream).await;
            let next_sequence = output["sequence"].as_u64().unwrap();
            assert!(next_sequence > sequence);
            sequence = next_sequence;
            if output["data"].as_str().unwrap().contains("after-snapshot") {
                break;
            }
        }
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn clear_waits_for_persistence_and_blocks_later_output_publication() {
        let (directory, manager) = fixture().await;
        manager.open(open_input(directory.path())).await.unwrap();
        let mut stream = manager.observe(observe_input()).await.unwrap();
        ready(&mut stream).await;
        let gate = manager.0.history.block_next_persist(Some(""));
        let clearing = manager.clone();
        let clear = tokio::spawn(async move { clearing.clear(observe_input()).await });
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        let (arrived, arrival) = tokio::sync::oneshot::channel();
        *manager.0.next_output.lock().unwrap() = Some(arrived);
        manager
            .write(write_input("ordered-after-clear\n"))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), arrival)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            stream.receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        gate.release.notify_one();
        clear.await.unwrap().unwrap();
        let cleared = next(&mut stream).await;
        assert_eq!(cleared["type"], "cleared");
        let output = next(&mut stream).await;
        assert_eq!(output["type"], "output");
        assert!(
            output["data"]
                .as_str()
                .unwrap()
                .contains("ordered-after-clear")
        );
        assert!(output["sequence"].as_u64().unwrap() > cleared["sequence"].as_u64().unwrap());
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn attach_running_session_ignores_missing_provider_and_context_but_open_restarts_changed_environment()
     {
        let (directory, manager) = fixture().await;
        let mut input = open_input(directory.path());
        input.env = Some(Some(
            serde_json::from_value(json!({"VALUE":"first"})).unwrap(),
        ));
        let first = manager.open(input).await.unwrap();
        let mut stream = manager.observe(observe_input()).await.unwrap();
        ready(&mut stream).await;
        manager.write(write_input("old-context\n")).await.unwrap();
        assert!(
            next(&mut stream).await["data"]
                .as_str()
                .unwrap()
                .contains("old-context")
        );
        let mut attached=manager.attach(serde_json::from_value(json!({"threadId":"thread-fixture","terminalId":"term-1","cwd":"/missing-fixture-cwd","providerInstanceId":"not-real","env":{"VALUE":"ignored"}})).unwrap()).await.unwrap();
        assert_eq!(
            next(&mut attached).await["snapshot"]["pid"],
            json!(first.pid)
        );
        let mut changed = open_input(directory.path());
        changed.env = Some(Some(
            serde_json::from_value(json!({"VALUE":"second"})).unwrap(),
        ));
        let second = manager.open(changed).await.unwrap();
        assert_ne!(first.pid, second.pid);
        assert!(!second.history.contains("old-context"));
        let mut current = manager.observe(observe_input()).await.unwrap();
        ready(&mut current).await;
        manager.write(write_input("ENV\n")).await.unwrap();
        assert_eq!(next(&mut current).await["data"], "ENV=second\n");
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn shutdown_reaps_running_terminal_and_rejects_later_open_without_starting_child() {
        let (directory, manager) = fixture().await;
        let opened = manager.open(open_input(directory.path())).await.unwrap();
        let pid = opened.pid.unwrap().0;
        manager.shutdown().await;
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        let error = manager
            .open(open_input(directory.path()))
            .await
            .unwrap_err();
        assert_eq!(error.wire()["_tag"], "TerminalNotRunningError");
        assert!(manager.0.sessions.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn concurrent_shutdown_waits_for_starting_owned_pty_and_reaps_before_returning() {
        let (directory, manager) = fixture().await;
        let gate = Arc::new(StartGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            pid: std::sync::atomic::AtomicU32::new(0),
        });
        *manager.0.next_start.lock().unwrap() = Some(gate.clone());
        let opening = manager.clone();
        let input = open_input(directory.path());
        let started = tokio::spawn(async move { opening.open(input).await });
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
            .await
            .unwrap();
        let pid = gate.pid.load(Ordering::SeqCst);
        assert!(pid > 0);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
        let closing = manager.clone();
        let shutdown = tokio::spawn(async move { closing.shutdown().await });
        tokio::time::timeout(
            Duration::from_secs(3),
            manager.0.shutdown_started.notified(),
        )
        .await
        .unwrap();
        assert!(!shutdown.is_finished());
        gate.release.notify_one();
        let snapshot = tokio::time::timeout(Duration::from_secs(3), started)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.status, TerminalSessionStatus::Error);
        tokio::time::timeout(Duration::from_secs(3), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        assert!(manager.0.sessions.lock().unwrap().is_empty());
    }
}
impl TerminalManagerOptions {
    pub fn host(logs_directory: PathBuf, settings: &ServerSettings) -> Self {
        Self {
            logs_directory,
            environment: std::env::vars().collect(),
            home_directory: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_default(),
            platform: if cfg!(windows) {
                "win32"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else {
                "linux"
            }
            .into(),
            shell: None,
            history_lines: 5000,
            history_bytes: 8 * 1024 * 1024,
            kill_grace: Duration::from_secs(1),
            retained_inactive: 128,
            provider_instances: Some(crate::provider_registry::derive_instance_configs(settings)),
        }
    }
}
type Key = (String, String);
struct Runtime {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
struct State {
    cwd: String,
    worktree: Option<String>,
    runtime_env: Option<Environment>,
    cols: u16,
    rows: u16,
    status: TerminalSessionStatus,
    pid: Option<u32>,
    exit_code: Option<i64>,
    exit_signal: Option<i64>,
    history: BoundedTerminalHistory,
    filter: TerminalHistoryFilter,
    sequence: u64,
    updated_at: String,
    io: Option<TerminalIo>,
}
struct Session {
    key: Key,
    state: Mutex<State>,
    runtime: tokio::sync::Mutex<Option<Runtime>>,
    persist: tokio::sync::Mutex<()>,
    event_order: tokio::sync::Mutex<()>,
}
struct Listener {
    key: Option<Key>,
    watermark: u64,
    sender: mpsc::Sender<Value>,
    overflow: Arc<AtomicBool>,
    raw: bool,
}
struct Inner {
    options: TerminalManagerOptions,
    history: TerminalHistoryStore,
    sessions: Mutex<HashMap<Key, Arc<Session>>>,
    locks: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    listeners: Mutex<HashMap<u64, Listener>>,
    next_listener: AtomicU64,
    closed: AtomicBool,
    #[cfg(test)]
    next_output: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    #[cfg(test)]
    next_start: Mutex<Option<Arc<StartGate>>>,
    #[cfg(test)]
    shutdown_started: tokio::sync::Notify,
}
#[cfg(test)]
struct StartGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    pid: std::sync::atomic::AtomicU32,
}
impl Drop for Inner {
    fn drop(&mut self) {
        for session in self.sessions.get_mut().unwrap().values() {
            if let Some(runtime) = session
                .runtime
                .try_lock()
                .ok()
                .and_then(|mut runtime| runtime.take())
            {
                runtime.stop.send_replace(true);
            }
        }
    }
}
#[derive(Clone)]
pub struct TerminalManager(Arc<Inner>);
pub struct TerminalSubscription {
    receiver: mpsc::Receiver<Value>,
    overflow: Arc<AtomicBool>,
    owner: Weak<Inner>,
    id: u64,
}
impl TerminalSubscription {
    pub async fn recv(&mut self) -> Result<Option<Value>, String> {
        let next = self.receiver.recv().await;
        if next.is_none() && self.overflow.swap(false, Ordering::SeqCst) {
            Err("Terminal event continuity was lost: stream buffer exceeded".into())
        } else {
            Ok(next)
        }
    }
}
impl Drop for TerminalSubscription {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.listeners.lock().unwrap().remove(&self.id);
        }
    }
}
fn stamp(state: &mut State) -> u64 {
    state.sequence += 1;
    state.updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    state.sequence
}
fn label(terminal: &str) -> String {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| {
            regex::Regex::new(
                r"(?i)^term(?:inal)?-([0-9]+)(?:-[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12})?$",
            )
            .unwrap()
        })
        .captures(terminal)
        .map(|captures| format!("Terminal {}", &captures[1]))
        .unwrap_or_else(|| terminal.into())
        .chars()
        .scan(0, |units, c| {
            *units += c.len_utf16();
            if *units <= 128 { Some(c) } else { None }
        })
        .collect()
}
fn snapshot(session: &Session) -> TerminalSessionSnapshot {
    let mut state = session.state.lock().unwrap();
    let history = state.history.value().to_owned();
    serde_json::from_value(json!({"threadId":session.key.0,"terminalId":session.key.1,"cwd":state.cwd,"worktreePath":state.worktree,"status":state.status,"pid":state.pid,"exitCode":state.exit_code,"exitSignal":state.exit_signal,"history":history,"sequence":state.sequence,"label":label(&session.key.1),"updatedAt":state.updated_at})).expect("validated native terminal snapshot")
}
fn event(session: &Session, kind: &str, extra: Value) -> Value {
    let mut state = session.state.lock().unwrap();
    let sequence = stamp(&mut state);
    let mut event = json!({"type":kind,"threadId":session.key.0,"terminalId":session.key.1,"sequence":sequence});
    event
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    event
}
impl Inner {
    fn evict_inactive(&self) {
        let mut sessions = self.sessions.lock().unwrap();
        let mut inactive: Vec<_> = sessions
            .values()
            .filter_map(|session| {
                let state = session.state.lock().unwrap();
                matches!(
                    state.status,
                    TerminalSessionStatus::Exited | TerminalSessionStatus::Error
                )
                .then(|| (state.updated_at.clone(), session.key.clone()))
            })
            .collect();
        inactive.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| crate::workspace_entries::collate(&left.1.0, &right.1.0))
                .then_with(|| crate::workspace_entries::collate(&left.1.1, &right.1.1))
        });
        let remove = inactive
            .len()
            .saturating_sub(self.options.retained_inactive);
        for (_, key) in inactive.into_iter().take(remove) {
            sessions.remove(&key);
        }
    }
    fn publish(&self, event: Value) {
        let key = (
            event["threadId"].as_str().unwrap().to_owned(),
            event["terminalId"].as_str().unwrap().to_owned(),
        );
        let sequence = event["sequence"].as_u64().unwrap();
        let mut listeners = self.listeners.lock().unwrap();
        listeners.retain(|_, listener| {
            let output = match &listener.key {
                Some(target) if target != &key || sequence <= listener.watermark => return true,
                Some(_) => {
                    if event["type"] == "started" {
                        json!({"type":"snapshot","snapshot":event["snapshot"]})
                    } else {
                        event.clone()
                    }
                }
                None if listener.raw => event.clone(),
                None => {
                    if matches!(event["type"].as_str(), Some("output" | "cleared")) {
                        return true;
                    }
                    if event["type"] == "closed" {
                        json!({"type":"remove","threadId":key.0,"terminalId":key.1})
                    } else if let Some(session) = self.sessions.lock().unwrap().get(&key).cloned() {
                        json!({"type":"upsert","terminal":self.summary(&session)})
                    } else {
                        return true;
                    }
                }
            };
            match listener.sender.try_send(output) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Closed(_)) => false,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    listener.overflow.store(true, Ordering::SeqCst);
                    false
                }
            }
        });
    }
    fn summary(&self, session: &Session) -> Value {
        // Metadata never materializes the retained (up to 8 MiB) history.
        let state = session.state.lock().unwrap();
        json!({"threadId":session.key.0,"terminalId":session.key.1,"cwd":state.cwd,"worktreePath":state.worktree,"status":state.status,"pid":state.pid,"exitCode":state.exit_code,"exitSignal":state.exit_signal,"label":label(&session.key.1),"updatedAt":state.updated_at,"hasRunningSubprocess":false})
    }
    async fn persist(&self, session: &Session) {
        let _serial = session.persist.lock().await;
        let history = session.state.lock().unwrap().history.value().to_owned();
        self.history
            .persist(&session.key.0, &session.key.1, history)
            .await;
    }
}
impl TerminalManager {
    pub async fn new(options: TerminalManagerOptions) -> io::Result<Self> {
        let history = TerminalHistoryStore::new(
            options.logs_directory.clone(),
            options.history_lines,
            options.history_bytes,
        )
        .await?;
        Ok(Self(Arc::new(Inner {
            options,
            history,
            sessions: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            listeners: Mutex::new(HashMap::new()),
            next_listener: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            next_output: Mutex::new(None),
            #[cfg(test)]
            next_start: Mutex::new(None),
            #[cfg(test)]
            shutdown_started: tokio::sync::Notify::new(),
        })))
    }
    async fn thread_lock(&self, thread: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.0.locks.lock().unwrap();
            locks.retain(|_, lock| lock.strong_count() > 0);
            let lock = locks
                .get(thread)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
            locks.insert(thread.into(), Arc::downgrade(&lock));
            lock
        };
        lock.lock_owned().await
    }
    fn session(&self, thread: &str, terminal: &str) -> Result<Arc<Session>, TerminalFailure> {
        self.0.sessions.lock().unwrap().get(&(thread.into(),terminal.into())).cloned().ok_or_else(||TerminalFailure::from_value(json!({"_tag":"TerminalSessionLookupError","threadId":thread,"terminalId":terminal})))
    }
    async fn valid_cwd(cwd: &str) -> Result<(), TerminalFailure> {
        match tokio::fs::metadata(cwd).await {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => Err(TerminalFailure::from_value(
                json!({"_tag":"TerminalCwdNotDirectoryError","cwd":cwd}),
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Err(
                TerminalFailure::from_value(json!({"_tag":"TerminalCwdNotFoundError","cwd":cwd})),
            ),
            Err(error) => Err(TerminalFailure::from_value(
                json!({"_tag":"TerminalCwdStatError","cwd":cwd,"cause":io_cause(error)}),
            )),
        }
    }
    fn environment(
        &self,
        provider: Option<&ProviderInstanceId>,
        env: Option<TerminalEnv>,
    ) -> Result<Option<Environment>, TerminalFailure> {
        let mut env: Environment = env
            .map(|env| {
                env.0
                    .into_iter()
                    .map(|(key, value)| (key, value.0))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(id) = provider {
            let instance=self.0.options.provider_instances.as_ref().and_then(|instances|instances.get(id)).ok_or_else(||TerminalFailure::from_value(json!({"_tag":"TerminalProviderInstanceNotFoundError","providerInstanceId":id})))?;
            if let Some(variables) = &instance.environment {
                for variable in variables {
                    env.insert(variable.name.to_string(), variable.value.clone());
                }
            }
            // Source decodes the optional driver config; invalid config simply
            // contributes no home override. Terminal setup need not launch it.
            let config = instance.config.clone().unwrap_or(json!({}));
            if instance.driver.as_str() == "codex" {
                if let Ok(config) = serde_json::from_value::<CodexSettings>(config) {
                    let home = if !config.shadow_home_path.as_str().is_empty() {
                        config.shadow_home_path.as_str()
                    } else {
                        config.home_path.as_str()
                    };
                    if !home.is_empty() {
                        env.insert("CODEX_HOME".into(), self.absolute_home(home));
                    }
                }
            } else if instance.driver.as_str() == "claudeAgent" {
                if let Ok(config) = serde_json::from_value::<ClaudeSettings>(config) {
                    if !config.home_path.as_str().is_empty() {
                        env.insert(
                            "CLAUDE_CONFIG_DIR".into(),
                            self.absolute_home(config.home_path.as_str()),
                        );
                    }
                }
            }
        }
        Ok(if env.is_empty() { None } else { Some(env) })
    }
    fn absolute_home(&self, path: &str) -> String {
        let mut runtime = Environment::new();
        runtime.insert("CODEX_HOME".into(), path.into());
        let expanded = spawn_environment(
            &Environment::new(),
            Some(&runtime),
            &self.0.options.platform,
            &self.0.options.home_directory,
        )
        .shift_remove("CODEX_HOME")
        .unwrap();
        crate::workspace_files::normalize(&PathBuf::from(expanded))
            .to_string_lossy()
            .into_owned()
    }
    async fn stop(&self, session: &Session) {
        if let Some(runtime) = session.runtime.lock().await.take() {
            runtime.stop.send_replace(true);
            if let Err(error) = runtime.task.await {
                tracing::error!(%error,"terminal output task failed during shutdown");
            }
        }
    }
    async fn start(&self, session: Arc<Session>, kind: &str) {
        let (cwd, cols, rows, environment) = {
            let mut state = session.state.lock().unwrap();
            state.status = TerminalSessionStatus::Starting;
            state.exit_code = None;
            state.exit_signal = None;
            (
                state.cwd.clone(),
                state.cols,
                state.rows,
                spawn_environment(
                    &self.0.options.environment,
                    state.runtime_env.as_ref(),
                    &self.0.options.platform,
                    &self.0.options.home_directory,
                ),
            )
        };
        let mut failure = None;
        let mut process = None;
        let candidates = shell_candidates(
            self.0.options.shell.as_deref(),
            &self.0.options.platform,
            &self.0.options.environment,
        );
        for candidate in &candidates {
            match TerminalProcess::spawn(
                PtySpawn {
                    shell: candidate.shell.clone().into(),
                    args: candidate.args.clone(),
                    cwd: cwd.clone().into(),
                    environment: environment.clone(),
                    cols,
                    rows,
                },
                self.0.options.kill_grace,
            )
            .await
            {
                Ok(child) => {
                    process = Some(child);
                    break;
                }
                Err(error) => {
                    let message = error.to_string().to_lowercase();
                    let retry = matches!(error.kind(), io::ErrorKind::NotFound)
                        || [
                            "posix_spawnp failed",
                            "enoent",
                            "not found",
                            "file not found",
                            "no such file",
                        ]
                        .iter()
                        .any(|needle| message.contains(needle));
                    failure = Some(error);
                    if !retry {
                        break;
                    }
                }
            }
        }
        let Some(mut process) = process else {
            {
                let mut state = session.state.lock().unwrap();
                state.status = TerminalSessionStatus::Error;
                state.pid = None;
                state.io = None;
            }
            let message = format!(
                "Failed to spawn PTY process with native adapter. Tried shells: {}. {}",
                candidates
                    .iter()
                    .map(|candidate| candidate.shell.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                failure.map(|error| error.to_string()).unwrap_or_default()
            );
            self.0
                .publish(event(&session, "error", json!({"message":message})));
            self.0.evict_inactive();
            return;
        };
        #[cfg(test)]
        {
            let gate = self.0.next_start.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.pid.store(process.pid, Ordering::SeqCst);
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
        if self.0.closed.load(Ordering::SeqCst) {
            if let Err(error) = process.close().await {
                tracing::warn!(%error,"failed to close terminal started during shutdown");
            }
            {
                let mut state = session.state.lock().unwrap();
                state.status = TerminalSessionStatus::Error;
                state.pid = None;
                state.io = None;
            }
            self.0.publish(event(
                &session,
                "error",
                json!({"message":"Terminal manager is shutting down."}),
            ));
            return;
        }
        {
            let mut state = session.state.lock().unwrap();
            state.status = TerminalSessionStatus::Running;
            state.pid = Some(process.pid);
            state.io = Some(process.io.clone());
            stamp(&mut state);
        }
        let initial = snapshot(&session);
        self.0.publish(json!({"type":kind,"threadId":session.key.0,"terminalId":session.key.1,"sequence":initial.sequence,"snapshot":initial}));
        // Publish startup first; the already-installed PTY reader retains any
        // immediate output/exit while lifecycle state and snapshot are committed.
        let (stop, mut stopped) = watch::channel(false);
        let owner = Arc::downgrade(&self.0);
        let running = session.clone();
        let task = tokio::spawn(async move {
            let mut dirty = false;
            let mut deadline = tokio::time::Instant::now() + Duration::from_millis(40);
            loop {
                let next = tokio::select! {
                    biased;
                    _=async {let _=stopped.wait_for(|stop|*stop).await;}=>break,
                    next=process.events.recv()=>next,
                    _=tokio::time::sleep_until(deadline),if dirty=>{
                        if let Some(owner)=owner.upgrade(){owner.persist(&running).await;}
                        dirty=false;continue;
                    }
                };
                let Some(next) = next else {
                    break;
                };
                #[cfg(test)]
                if matches!(next, PtyEvent::Output(_)) {
                    if let Some(owner) = owner.upgrade() {
                        if let Some(arrived) = owner.next_output.lock().unwrap().take() {
                            let _ = arrived.send(());
                        }
                    }
                }
                let _order = running.event_order.lock().await;
                if *stopped.borrow() {
                    break;
                }
                let Some(owner) = owner.upgrade() else {
                    break;
                };
                match next {
                    PtyEvent::Output(data) => {
                        let sequence = {
                            let mut state = running.state.lock().unwrap();
                            let visible = state.filter.feed(&data);
                            if !visible.is_empty() {
                                state.history.append(&visible);
                                if !dirty {
                                    deadline =
                                        tokio::time::Instant::now() + Duration::from_millis(40);
                                }
                                dirty = true;
                            }
                            stamp(&mut state)
                        };
                        owner.publish(json!({"type":"output","threadId":running.key.0,"terminalId":running.key.1,"sequence":sequence,"data":data}));
                    }
                    PtyEvent::Exited(exit) => {
                        {
                            let mut state = running.state.lock().unwrap();
                            state.status = TerminalSessionStatus::Exited;
                            state.pid = None;
                            state.io = None;
                            state.exit_code = Some(exit.code);
                            state.exit_signal = exit.signal;
                            state.filter = TerminalHistoryFilter::default();
                        }
                        owner.publish(event(
                            &running,
                            "exited",
                            json!({"exitCode":exit.code,"exitSignal":exit.signal}),
                        ));
                        break;
                    }
                    PtyEvent::Error(message) => {
                        {
                            let mut state = running.state.lock().unwrap();
                            state.status = TerminalSessionStatus::Error;
                            state.pid = None;
                            state.io = None;
                        }
                        owner.publish(event(&running, "error", json!({"message":message})));
                        break;
                    }
                }
            }
            if let Err(error) = process.close().await {
                tracing::warn!(%error,"failed to close owned terminal process");
            }
            {
                let mut state = running.state.lock().unwrap();
                state.pid = None;
                state.io = None;
                if matches!(
                    state.status,
                    TerminalSessionStatus::Running | TerminalSessionStatus::Starting
                ) {
                    state.status = TerminalSessionStatus::Exited;
                    state.updated_at =
                        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                }
            }
            if let Some(owner) = owner.upgrade() {
                owner.persist(&running).await;
                owner.evict_inactive();
            }
        });
        *session.runtime.lock().await = Some(Runtime { stop, task });
    }
    pub async fn open(
        &self,
        input: TerminalOpenInput,
    ) -> Result<TerminalSessionSnapshot, TerminalFailure> {
        let _thread = self.thread_lock(input.thread_id.as_str()).await;
        let env = self.environment(
            input.provider_instance_id.as_ref().and_then(Option::as_ref),
            input.env.flatten(),
        )?;
        self.open_resolved(
            input.thread_id.to_string(),
            input.terminal_id.0.to_string(),
            input.cwd.to_string(),
            input
                .worktree_path
                .map(|path| path.map(|path| path.to_string())),
            input.cols.flatten().map(|cols| cols.0 as u16),
            input.rows.flatten().map(|rows| rows.0 as u16),
            env,
            false,
        )
        .await
    }
    async fn open_resolved(
        &self,
        thread: String,
        terminal: String,
        cwd: String,
        worktree: Option<Option<String>>,
        cols: Option<u16>,
        rows: Option<u16>,
        env: Option<Environment>,
        restart: bool,
    ) -> Result<TerminalSessionSnapshot, TerminalFailure> {
        Self::valid_cwd(&cwd).await?;
        if self.0.closed.load(Ordering::SeqCst) {
            return Err(TerminalFailure::from_value(
                json!({"_tag":"TerminalNotRunningError","threadId":thread,"terminalId":terminal}),
            ));
        }
        let key = (thread.clone(), terminal.clone());
        let existing = self.0.sessions.lock().unwrap().get(&key).cloned();
        let session = if let Some(session) = existing {
            session
        } else {
            let history = if restart {
                BoundedTerminalHistory::new(
                    self.0.options.history_lines,
                    "",
                    self.0.options.history_bytes,
                )
            } else {
                self.0.history.read(&thread, &terminal).await?
            };
            let session = Arc::new(Session {
                key: key.clone(),
                state: Mutex::new(State {
                    cwd: cwd.clone(),
                    worktree: worktree.clone().flatten(),
                    runtime_env: env.clone(),
                    cols: cols.unwrap_or(120),
                    rows: rows.unwrap_or(30),
                    status: TerminalSessionStatus::Starting,
                    pid: None,
                    exit_code: None,
                    exit_signal: None,
                    history,
                    filter: TerminalHistoryFilter::default(),
                    sequence: 0,
                    updated_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    io: None,
                }),
                runtime: tokio::sync::Mutex::new(None),
                persist: tokio::sync::Mutex::new(()),
                event_order: tokio::sync::Mutex::new(()),
            });
            {
                let mut sessions = self.0.sessions.lock().unwrap();
                if self.0.closed.load(Ordering::SeqCst) {
                    return Err(TerminalFailure::from_value(
                        json!({"_tag":"TerminalNotRunningError","threadId":thread,"terminalId":terminal}),
                    ));
                }
                sessions.insert(key, session.clone());
            }
            session
        };
        let (changed, not_running, io, old_cols, old_rows) = {
            let state = session.state.lock().unwrap();
            (
                state.cwd != cwd
                    || state.runtime_env != env
                    || worktree
                        .as_ref()
                        .is_some_and(|worktree| worktree != &state.worktree),
                state.status != TerminalSessionStatus::Running,
                state.io.clone(),
                state.cols,
                state.rows,
            )
        };
        if restart || changed || not_running {
            self.stop(&session).await;
            {
                let mut state = session.state.lock().unwrap();
                state.cwd = cwd;
                if let Some(worktree) = worktree {
                    state.worktree = worktree;
                }
                state.runtime_env = env;
                state.cols = cols.unwrap_or(old_cols);
                state.rows = rows.unwrap_or(old_rows);
                if restart
                    || changed
                    || matches!(
                        state.status,
                        TerminalSessionStatus::Exited | TerminalSessionStatus::Error
                    )
                {
                    state.history.clear();
                    state.filter = TerminalHistoryFilter::default();
                }
            }
            if restart || changed {
                self.0.persist(&session).await;
            }
            self.start(
                session.clone(),
                if restart { "restarted" } else { "started" },
            )
            .await;
        } else if let Some(io) = io {
            let cols = cols.unwrap_or(old_cols);
            let rows = rows.unwrap_or(old_rows);
            if (cols, rows) != (old_cols, old_rows) {
                self.resize_session(&session, io, cols, rows).await?;
            }
        }
        Ok(snapshot(&session))
    }
    async fn resize_session(
        &self,
        session: &Session,
        io: TerminalIo,
        cols: u16,
        rows: u16,
    ) -> Result<(), TerminalFailure> {
        let pid = session.state.lock().unwrap().pid.unwrap_or(0);
        io.resize(cols,rows).await.map_err(|error|TerminalFailure::from_value(json!({"_tag":"TerminalResizeError","threadId":session.key.0,"terminalId":session.key.1,"terminalPid":pid,"cols":cols,"rows":rows,"cause":io_cause(error)})))?;
        let mut state = session.state.lock().unwrap();
        state.cols = cols;
        state.rows = rows;
        state.updated_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        Ok(())
    }
    pub async fn write(&self, input: TerminalWriteInput) -> Result<(), TerminalFailure> {
        let session = self.session(input.thread_id.as_str(), input.terminal_id.0.as_str())?;
        let (io, status, pid) = {
            let state = session.state.lock().unwrap();
            (state.io.clone(), state.status, state.pid)
        };
        if status == TerminalSessionStatus::Exited {
            return Ok(());
        }
        let io=io.ok_or_else(||TerminalFailure::from_value(json!({"_tag":"TerminalNotRunningError","threadId":input.thread_id,"terminalId":input.terminal_id})))?;
        io.write(input.data.0).await.map_err(|error|TerminalFailure::from_value(json!({"_tag":"TerminalWriteError","threadId":input.thread_id,"terminalId":input.terminal_id,"terminalPid":pid.unwrap_or(0),"cause":io_cause(error)})))
    }
    pub async fn resize(&self, input: TerminalResizeInput) -> Result<(), TerminalFailure> {
        let _thread = self.thread_lock(input.thread_id.as_str()).await;
        if let Ok(session) = self.session(input.thread_id.as_str(), input.terminal_id.0.as_str()) {
            let io = session.state.lock().unwrap().io.clone();
            if let Some(io) = io {
                self.resize_session(&session, io, input.cols.0 as u16, input.rows.0 as u16)
                    .await?;
            }
        }
        Ok(())
    }
    pub async fn clear(&self, input: TerminalClearInput) -> Result<(), TerminalFailure> {
        let _thread = self.thread_lock(input.thread_id.as_str()).await;
        let session = self.session(input.thread_id.as_str(), input.terminal_id.0.as_str())?;
        let _order = session.event_order.lock().await;
        {
            let mut state = session.state.lock().unwrap();
            state.history.clear();
            state.filter = TerminalHistoryFilter::default();
        }
        let cleared = event(&session, "cleared", json!({}));
        self.0.persist(&session).await;
        self.0.publish(cleared);
        Ok(())
    }
    pub async fn restart(
        &self,
        input: TerminalRestartInput,
    ) -> Result<TerminalSessionSnapshot, TerminalFailure> {
        let _thread = self.thread_lock(input.thread_id.as_str()).await;
        let env = self.environment(
            input.provider_instance_id.as_ref().and_then(Option::as_ref),
            input.env.flatten(),
        )?;
        self.open_resolved(
            input.thread_id.to_string(),
            input.terminal_id.0.to_string(),
            input.cwd.to_string(),
            Some(input.worktree_path.flatten().map(|path| path.to_string())),
            Some(input.cols.0 as u16),
            Some(input.rows.0 as u16),
            env,
            true,
        )
        .await
    }
    async fn close_session(&self, session: Arc<Session>, delete: bool) {
        self.stop(&session).await;
        self.0.persist(&session).await;
        let closed = event(&session, "closed", json!({}));
        self.0.sessions.lock().unwrap().remove(&session.key);
        self.0.publish(closed);
        if delete {
            self.0.history.delete(&session.key.0, &session.key.1).await;
        }
    }
    pub async fn close(&self, input: TerminalCloseInput) -> Result<(), TerminalFailure> {
        let _thread = self.thread_lock(input.thread_id.as_str()).await;
        let terminal = input.terminal_id.flatten();
        let delete = input.delete_history.flatten() == Some(true);
        let sessions: Vec<_> = self
            .0
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|session| {
                session.key.0 == input.thread_id.as_str()
                    && terminal
                        .as_ref()
                        .is_none_or(|terminal| terminal.0.as_str() == session.key.1)
            })
            .cloned()
            .collect();
        for session in sessions {
            self.close_session(session, delete && terminal.is_some())
                .await;
        }
        if delete {
            if let Some(terminal) = terminal {
                self.0
                    .history
                    .delete(input.thread_id.as_str(), terminal.0.as_str())
                    .await;
            } else {
                self.0.history.delete_thread(input.thread_id.as_str()).await;
            }
        }
        Ok(())
    }
    fn subscription(&self, session: &Session) -> TerminalSubscription {
        let mut listeners = self.0.listeners.lock().unwrap();
        let initial = snapshot(session);
        let watermark = initial
            .sequence
            .flatten()
            .map(|sequence| sequence.0 as u64)
            .unwrap();
        let (sender, receiver) = mpsc::channel(64);
        let overflow = Arc::new(AtomicBool::new(false));
        sender
            .try_send(json!({"type":"snapshot","snapshot":initial}))
            .unwrap();
        let id = self.0.next_listener.fetch_add(1, Ordering::SeqCst);
        listeners.insert(
            id,
            Listener {
                key: Some(session.key.clone()),
                watermark,
                sender,
                overflow: overflow.clone(),
                raw: false,
            },
        );
        TerminalSubscription {
            receiver,
            overflow,
            owner: Arc::downgrade(&self.0),
            id,
        }
    }
    pub async fn observe(
        &self,
        input: TerminalObserveInput,
    ) -> Result<TerminalSubscription, TerminalFailure> {
        let _thread = self.thread_lock(input.thread_id.as_str()).await;
        let session = self.session(input.thread_id.as_str(), input.terminal_id.0.as_str())?;
        Ok(self.subscription(&session))
    }
    pub async fn attach(
        &self,
        input: TerminalAttachInput,
    ) -> Result<TerminalSubscription, TerminalFailure> {
        let _thread = self.thread_lock(input.thread_id.as_str()).await;
        let existing = self
            .session(input.thread_id.as_str(), input.terminal_id.0.as_str())
            .ok();
        let needs_open = existing.as_ref().is_none_or(|session| {
            session.state.lock().unwrap().io.is_none()
                && input.cwd.as_ref().and_then(Option::as_ref).is_some()
                && input.restart_if_not_running.flatten() == Some(true)
        });
        if needs_open {
            let cwd=input.cwd.as_ref().and_then(Option::as_ref).ok_or_else(||TerminalFailure::from_value(json!({"_tag":"TerminalSessionLookupError","threadId":input.thread_id,"terminalId":input.terminal_id})))?;
            let env = self.environment(
                input.provider_instance_id.as_ref().and_then(Option::as_ref),
                input.env.flatten(),
            )?;
            self.open_resolved(
                input.thread_id.to_string(),
                input.terminal_id.0.to_string(),
                cwd.to_string(),
                input
                    .worktree_path
                    .map(|path| path.map(|path| path.to_string())),
                input.cols.flatten().map(|cols| cols.0 as u16),
                input.rows.flatten().map(|rows| rows.0 as u16),
                env,
                false,
            )
            .await?;
        } else if let Some(session) = &existing {
            // Attaching a running session neither re-resolves provider credentials
            // nor changes its cwd/environment; only dimensions are applied.
            let (io, cols, rows, changed) = {
                let state = session.state.lock().unwrap();
                let cols = input
                    .cols
                    .flatten()
                    .map(|cols| cols.0 as u16)
                    .unwrap_or(state.cols);
                let rows = input
                    .rows
                    .flatten()
                    .map(|rows| rows.0 as u16)
                    .unwrap_or(state.rows);
                (
                    state.io.clone(),
                    cols,
                    rows,
                    (state.cols, state.rows) != (cols, rows),
                )
            };
            if let Some(io) = io.filter(|_| changed) {
                self.resize_session(session, io, cols, rows).await?;
            }
        }
        let session = self.session(input.thread_id.as_str(), input.terminal_id.0.as_str())?;
        Ok(self.subscription(&session))
    }
    pub fn metadata(&self) -> TerminalSubscription {
        let mut listeners = self.0.listeners.lock().unwrap();
        let mut terminals: Vec<_> = self
            .0
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|session| self.0.summary(session))
            .collect();
        terminals.sort_by(|a, b| {
            crate::workspace_entries::collate(
                b["updatedAt"].as_str().unwrap(),
                a["updatedAt"].as_str().unwrap(),
            )
            .then_with(|| {
                crate::workspace_entries::collate(
                    a["threadId"].as_str().unwrap(),
                    b["threadId"].as_str().unwrap(),
                )
            })
            .then_with(|| {
                crate::workspace_entries::collate(
                    a["terminalId"].as_str().unwrap(),
                    b["terminalId"].as_str().unwrap(),
                )
            })
        });
        let (sender, receiver) = mpsc::channel(64);
        sender
            .try_send(json!({"type":"snapshot","terminals":terminals}))
            .unwrap();
        let overflow = Arc::new(AtomicBool::new(false));
        let id = self.0.next_listener.fetch_add(1, Ordering::SeqCst);
        listeners.insert(
            id,
            Listener {
                key: None,
                watermark: 0,
                sender,
                overflow: overflow.clone(),
                raw: false,
            },
        );
        TerminalSubscription {
            receiver,
            overflow,
            owner: Arc::downgrade(&self.0),
            id,
        }
    }
    pub async fn shutdown(&self) {
        let sessions: Vec<_> = {
            let sessions = self.0.sessions.lock().unwrap();
            self.0.closed.store(true, Ordering::SeqCst);
            sessions.values().cloned().collect()
        };
        #[cfg(test)]
        self.0.shutdown_started.notify_one();
        for session in sessions {
            let _thread = self.thread_lock(&session.key.0).await;
            self.close_session(session, false).await;
        }
        self.0.listeners.lock().unwrap().clear();
    }
    pub fn events(&self) -> TerminalSubscription {
        let (sender, receiver) = mpsc::channel(64);
        let overflow = Arc::new(AtomicBool::new(false));
        let id = self.0.next_listener.fetch_add(1, Ordering::SeqCst);
        self.0.listeners.lock().unwrap().insert(
            id,
            Listener {
                key: None,
                watermark: 0,
                sender,
                overflow: overflow.clone(),
                raw: true,
            },
        );
        TerminalSubscription {
            receiver,
            overflow,
            owner: Arc::downgrade(&self.0),
            id,
        }
    }
}
