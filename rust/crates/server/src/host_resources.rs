//! Source HostResources: one demand-driven, five-second cache shared by sockets.
//! The TTL starts at completion. The last canceled waiter detaches and cancels
//! its lookup, so a subsequent caller never joins abandoned work.
use crate::{desktop_telemetry::Clock, terminal_inspector::NativeProcessTable};
use futures_util::future::BoxFuture;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use t3_contracts::HostResourcesSnapshot;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Clone, Copy, Debug, Default)]
pub struct Cpu {
    pub idle: f64,
    pub total: f64,
    pub count: u64,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Memory {
    pub total: f64,
    pub available: f64,
}
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct HostResourcesError(pub String);
type Outcome = Result<HostResourcesSnapshot, HostResourcesError>;
pub struct AvailableMemory {
    pub read: BoxFuture<'static, Option<f64>>,
    pub cleanup: BoxFuture<'static, ()>,
}
impl AvailableMemory {
    pub fn value(read: BoxFuture<'static, Option<f64>>) -> Self {
        Self {
            read,
            cleanup: Box::pin(async {}),
        }
    }
}
pub struct HostResourcesOptions {
    pub cpu: Arc<dyn Fn() -> Cpu + Send + Sync>,
    pub memory: Arc<dyn Fn() -> Memory + Send + Sync>,
    pub available: Arc<dyn Fn() -> AvailableMemory + Send + Sync>,
    pub clock: Clock,
    pub sample_interval: Duration,
    pub ttl: Duration,
}
impl HostResourcesOptions {
    pub fn host(clock: Clock) -> Self {
        Self {
            cpu: Arc::new(crate::host_system::cpu),
            memory: Arc::new(crate::host_system::memory),
            available: Arc::new(crate::host_system::available),
            clock,
            sample_interval: Duration::from_millis(200),
            ttl: Duration::from_secs(5),
        }
    }
}
struct Entry {
    result: watch::Sender<Option<Outcome>>,
    cancel: watch::Sender<bool>,
    waiters: std::sync::atomic::AtomicUsize,
    // Written and read only while Shared.state is locked.
    expires: Mutex<Option<i64>>,
}
struct Shared {
    state: Mutex<Option<Arc<Entry>>>,
    stop: watch::Sender<bool>,
    options: HostResourcesOptions,
}
struct Inner {
    shared: Arc<Shared>,
    tasks: Mutex<Option<Vec<JoinHandle<()>>>>,
    shutdown: tokio::sync::Mutex<Vec<JoinHandle<()>>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.stop.send_replace(true);
    }
}
#[derive(Clone)]
pub struct HostResources(Arc<Inner>);
struct Waiter {
    shared: Arc<Shared>,
    entry: Arc<Entry>,
}
impl Drop for Waiter {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        if self
            .entry
            .waiters
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
            == 1
            && self.entry.result.borrow().is_none()
        {
            if state
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.entry))
            {
                *state = None;
            }
            self.entry.cancel.send_replace(true);
        }
    }
}
impl HostResources {
    pub fn new(options: HostResourcesOptions) -> Self {
        let (stop, _) = watch::channel(false);
        Self(Arc::new(Inner {
            shared: Arc::new(Shared {
                state: Mutex::new(None),
                stop,
                options,
            }),
            tasks: Mutex::new(Some(Vec::new())),
            shutdown: tokio::sync::Mutex::new(Vec::new()),
        }))
    }
    pub async fn read(&self) -> Outcome {
        let shared = &self.0.shared;
        let (guard, mut result) = {
            let mut tasks = self.0.tasks.lock().unwrap();
            let Some(tasks) = tasks.as_mut() else {
                return Err(HostResourcesError("Host resources is shut down".into()));
            };
            if *shared.stop.borrow() {
                return Err(HostResourcesError("Host resources is shut down".into()));
            }
            let mut state = shared.state.lock().unwrap();
            if state.as_ref().is_some_and(|entry| {
                entry
                    .expires
                    .lock()
                    .unwrap()
                    .is_some_and(|expires| (shared.options.clock)() >= expires)
            }) {
                *state = None;
            }
            let entry = if let Some(entry) = &*state {
                entry.clone()
            } else {
                let (result, _) = watch::channel(None);
                let (cancel, cancelled) = watch::channel(false);
                let entry = Arc::new(Entry {
                    result,
                    cancel,
                    waiters: std::sync::atomic::AtomicUsize::new(0),
                    expires: Mutex::new(None),
                });
                *state = Some(entry.clone());
                let owned = shared.clone();
                let work = entry.clone();
                tasks.retain(|task| !task.is_finished());
                tasks.push(tokio::spawn(async move {
                    lookup(owned, work, cancelled).await;
                }));
                entry
            };
            entry
                .waiters
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = entry.result.subscribe();
            (
                Waiter {
                    shared: shared.clone(),
                    entry,
                },
                result,
            )
        };
        let outcome = loop {
            if let Some(outcome) = result.borrow_and_update().clone() {
                break outcome;
            }
            if result.changed().await.is_err() {
                break Err(HostResourcesError(
                    "Host sample ended without a result".into(),
                ));
            }
        };
        drop(guard);
        outcome
    }
    pub async fn shutdown(&self) {
        self.0.shared.stop.send_replace(true);
        let mut shutdown = self.0.shutdown.lock().await;
        if let Some(tasks) = self.0.tasks.lock().unwrap().take() {
            shutdown.extend(tasks);
        }
        for task in shutdown.iter_mut() {
            let _ = task.await;
        }
        shutdown.clear();
    }
}
async fn lookup(shared: Arc<Shared>, entry: Arc<Entry>, mut cancelled: watch::Receiver<bool>) {
    let mut stop = shared.stop.subscribe();
    let available = (shared.options.available)();
    let result = tokio::select! { biased;
        _ = stop.wait_for(|stopped| *stopped) => Err(HostResourcesError("Host resources is shut down".into())),
        _ = cancelled.wait_for(|cancelled| *cancelled) => Err(HostResourcesError("Host sample was canceled".into())),
        result = sample(&shared.options, available.read) => result,
    };
    available.cleanup.await;
    let state = shared.state.lock().unwrap();
    if state
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, &entry))
    {
        *entry.expires.lock().unwrap() = Some(
            (shared.options.clock)()
                .saturating_add(shared.options.ttl.as_millis().min(i64::MAX as u128) as i64),
        );
    }
    entry.result.send_replace(Some(result));
}
pub fn cpu_utilization(previous: Cpu, cpu: Cpu) -> Option<f64> {
    let total = cpu.total - previous.total;
    let idle = cpu.idle - previous.idle;
    (previous.count == cpu.count && total > 0. && idle >= 0.)
        .then(|| (1. - idle / total).clamp(0., 1.))
}
async fn sample(
    options: &HostResourcesOptions,
    available: BoxFuture<'static, Option<f64>>,
) -> Outcome {
    let previous = (options.cpu)();
    tokio::time::sleep(options.sample_interval).await;
    let cpu = (options.cpu)();
    let memory = (options.memory)();
    let available = available
        .await
        .unwrap_or(memory.available)
        .clamp(0., memory.total);
    serde_json::from_value(serde_json::json!({"sampledAt":(options.clock)(),"cpuUtilization":cpu_utilization(previous,cpu),"cpuCount":cpu.count,"totalMemoryBytes":memory.total,"availableMemoryBytes":available})).map_err(|error| HostResourcesError(error.to_string()))
}

pub fn darwin_available_memory(output: &str) -> Option<f64> {
    use std::sync::LazyLock;
    static PAGE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"page size of ([0-9]+) bytes").unwrap());
    static COUNTS: LazyLock<[regex::Regex; 3]> = LazyLock::new(|| {
        ["free", "inactive", "speculative"]
            .map(|name| regex::Regex::new(&r"(?m)^Pages {name}:[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]+([0-9]+)\.".replace("{name}", name)).unwrap())
    });
    let output = output.replace(['\r', '\u{2028}', '\u{2029}'], "\n");
    let page = PAGE.captures(&output)?[1].parse::<f64>().ok()?;
    let mut pages = 0.;
    for count in COUNTS.iter() {
        pages += count.captures(&output)?[1].parse::<f64>().ok()?;
    }
    let available = pages * page;
    (page > 0. && available.fract() == 0. && available <= 9_007_199_254_740_991.)
        .then_some(available)
}
pub fn linux_available_memory(output: &str) -> Option<f64> {
    static AVAILABLE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?m)^MemAvailable:[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]+([0-9]+)[\t\n\x0B\f\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]+kB$").unwrap()
    });
    let output = output.replace(['\r', '\u{2028}', '\u{2029}'], "\n");
    Some(AVAILABLE.captures(&output)?[1].parse::<f64>().ok()? * 1024.)
}
pub(crate) async fn vm_stat(command: NativeProcessTable) -> Option<f64> {
    let output = command.output().await.ok();
    let output = output?;
    // ChildProcessSpawner.string collects stdout independently of exit status.
    darwin_available_memory(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::Instant;
    fn options(calls: Arc<AtomicUsize>, started: Arc<tokio::sync::Notify>) -> HostResourcesOptions {
        let origin = Instant::now();
        HostResourcesOptions {
            cpu: Arc::new(move || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                started.notify_one();
                Cpu {
                    idle: n as f64 * 25.,
                    total: n as f64 * 100.,
                    count: 2,
                }
            }),
            memory: Arc::new(|| Memory {
                total: 1024.,
                available: 512.,
            }),
            available: Arc::new(|| AvailableMemory::value(Box::pin(async { None }))),
            clock: Arc::new(move || 1234 + origin.elapsed().as_millis() as i64),
            sample_interval: Duration::from_millis(200),
            ttl: Duration::from_secs(5),
        }
    }
    #[test]
    fn host_resources_helpers_match_source_oracle() {
        for line in include_str!("../tests/fixtures/host-resources.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            let result = match row["op"].as_str().unwrap() {
                "darwin" | "vm_stat_nonzero" => {
                    json!(darwin_available_memory(row["input"].as_str().unwrap()))
                }
                "linux" => json!(linux_available_memory(row["input"].as_str().unwrap())),
                "cpu" => {
                    let cpu = |v: &Value| Cpu {
                        idle: v["idle"].as_f64().unwrap(),
                        total: v["total"].as_f64().unwrap(),
                        count: v["count"].as_u64().unwrap(),
                    };
                    json!(cpu_utilization(cpu(&row["before"]), cpu(&row["after"])))
                }
                "cache" => {
                    assert_eq!(row["result"]["sharedCalls"], 1);
                    assert_eq!(row["result"]["calls"], 2);
                    assert_eq!(row["result"]["expires"], json!([5200]));
                    continue;
                }
                other => panic!("Unknown source witness {other}"),
            };
            crate::resource_model::tests::same_numbers(&result, &row["result"], &row.to_string());
        }
    }
    #[tokio::test(start_paused = true)]
    async fn shared_sample_survives_one_canceled_waiter_and_expires_from_completion() {
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let host = HostResources::new(options(calls.clone(), started.clone()));
        let mut a = Box::pin(host.read());
        assert!(futures_util::poll!(a.as_mut()).is_pending());
        started.notified().await;
        let mut b = Box::pin(host.read());
        assert!(futures_util::poll!(b.as_mut()).is_pending());
        assert_eq!(
            host.0
                .shared
                .state
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .waiters
                .load(Ordering::SeqCst),
            2
        );
        drop(a);
        tokio::time::advance(Duration::from_millis(200)).await;
        let value = b.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            serde_json::to_value(&value).unwrap()["cpuUtilization"],
            0.75
        );
        tokio::time::advance(Duration::from_millis(4999)).await;
        assert_eq!(
            serde_json::to_value(host.read().await.unwrap()).unwrap(),
            serde_json::to_value(value).unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_millis(1)).await;
        let mut next = Box::pin(host.read());
        assert!(futures_util::poll!(next.as_mut()).is_pending());
        started.notified().await;
        let _ = next.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        host.shutdown().await;
        assert!(host.read().await.is_err());
    }
    #[tokio::test(start_paused = true)]
    async fn last_waiter_detaches_abandoned_lookup_before_cleanup_and_new_lookup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let cleanup = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut options = options(calls.clone(), started.clone());
        options.available = Arc::new({
            let cleanup = cleanup.clone();
            let release = release.clone();
            move || {
                let cleanup = cleanup.clone();
                let release = release.clone();
                AvailableMemory {
                    read: Box::pin(async { std::future::pending().await }),
                    cleanup: Box::pin(async move {
                        cleanup.notify_one();
                        release.notified().await;
                    }),
                }
            }
        });
        let host = HostResources::new(options);
        let mut a = Box::pin(host.read());
        assert!(futures_util::poll!(a.as_mut()).is_pending());
        started.notified().await;
        drop(a);
        assert!(host.0.shared.state.lock().unwrap().is_none());
        cleanup.notified().await;
        let mut b = Box::pin(host.read());
        assert!(futures_util::poll!(b.as_mut()).is_pending());
        started.notified().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(b);
        cleanup.notified().await;
        release.notify_waiters();
        host.shutdown().await;
    }
    #[tokio::test]
    async fn actual_host_snapshot_is_typed_and_shared() {
        let host = HostResources::new(HostResourcesOptions::host(Arc::new(|| {
            chrono::Utc::now().timestamp_millis()
        })));
        let (a, b) = tokio::join!(host.read(), host.read());
        let a = serde_json::to_value(a.unwrap()).unwrap();
        assert_eq!(a, serde_json::to_value(b.unwrap()).unwrap());
        assert!(a["cpuCount"].as_u64().unwrap() > 0);
        assert!(a["totalMemoryBytes"].as_u64().unwrap() > 0);
        host.shutdown().await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_vm_stat_exit_keeps_valid_stdout_like_source_spawner() {
        let row: Value = include_str!("../tests/fixtures/host-resources.jsonl")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|row| row["op"] == "vm_stat_nonzero")
            .unwrap();
        let command = NativeProcessTable::command(
            "/bin/sh".into(),
            vec![
                "-c".into(),
                "printf '%s' \"$1\"; exit 7".into(),
                "vm-stat-fixture".into(),
                row["input"].as_str().unwrap().to_owned(),
            ],
            "vm_stat",
            Duration::from_secs(1),
            1_048_576,
        );
        let value = vm_stat(command.clone()).await;
        command.shutdown().await;
        crate::resource_model::tests::same_numbers(
            &json!(value),
            &row["result"],
            "nonzero vm_stat stdout",
        );
    }
    #[cfg(unix)]
    async fn held_command(timeout: bool) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("started.sock");
        let socket = tokio::net::UnixDatagram::bind(&socket_path).unwrap();
        let command=NativeProcessTable::command("python3".into(),vec!["-c".into(),"import os,socket,sys,signal;s=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM);s.sendto(str(os.getpid()).encode(),sys.argv[1]);signal.pause()".into(),socket_path.display().to_string()],"vm_stat",if timeout{Duration::from_secs(1)}else{Duration::from_secs(60)},1024);
        let mut options = options(
            Arc::new(AtomicUsize::new(0)),
            Arc::new(tokio::sync::Notify::new()),
        );
        options.sample_interval = Duration::ZERO;
        options.available = Arc::new(move || {
            let read = command.clone();
            let cleanup = command.clone();
            AvailableMemory {
                read: Box::pin(vm_stat(read)),
                cleanup: Box::pin(async move {
                    cleanup.shutdown().await;
                }),
            }
        });
        let host = HostResources::new(options);
        let owned = host.clone();
        let read = tokio::spawn(async move { owned.read().await });
        let mut buf = [0; 64];
        let len = tokio::time::timeout(Duration::from_secs(5), socket.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let pid: i32 = std::str::from_utf8(&buf[..len]).unwrap().parse().unwrap();
        if timeout {
            let value = read.await.unwrap().unwrap();
            assert_eq!(
                serde_json::to_value(value).unwrap()["availableMemoryBytes"],
                512
            );
        } else {
            read.abort();
            assert!(read.await.is_err());
        }
        host.shutdown().await;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn canceled_vm_stat_is_reaped_before_host_shutdown_returns() {
        held_command(false).await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn timed_out_vm_stat_is_reaped_and_uses_memory_fallback() {
        held_command(true).await;
    }
}
