//! Resolve SSH aliases without connecting. A failed lookup remains a remote
//! destination; proxies and forwarded local ports are never filtered out.
use crate::terminal_inspector::NativeProcessTable;
use futures_util::{StreamExt, future::BoxFuture, stream};
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv6Addr},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use t3_contracts::{SshDeviceHostConfig, base::trim_wire_string};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};

pub type AddressLookup = Arc<dyn Fn(String) -> BoxFuture<'static, Vec<String>> + Send + Sync>;
pub struct DeviceHostResolverOptions {
    pub ssh_binary: PathBuf,
    pub ssh_timeout: Duration,
    pub dns_timeout: Duration,
    pub lookup: AddressLookup,
    pub local_addresses: Arc<dyn Fn() -> HashSet<String> + Send + Sync>,
}
impl Default for DeviceHostResolverOptions {
    fn default() -> Self {
        Self {
            ssh_binary: if cfg!(windows) { "ssh.exe" } else { "ssh" }.into(),
            ssh_timeout: Duration::from_secs(5),
            dns_timeout: Duration::from_secs(2),
            lookup: Arc::new(|hostname| {
                Box::pin(async move {
                    tokio::net::lookup_host((hostname.as_str(), 0))
                        .await
                        .map(|addresses| {
                            addresses.map(|address| address.ip().to_string()).collect()
                        })
                        .unwrap_or_default()
                })
            }),
            local_addresses: Arc::new(|| {
                if_addrs::get_if_addrs()
                    .map(|addresses| {
                        addresses
                            .into_iter()
                            .map(|address| address.ip().to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            }),
        }
    }
}
struct Job {
    cancel: watch::Sender<bool>,
    handle: JoinHandle<()>,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}
struct Shared {
    options: Arc<DeviceHostResolverOptions>,
    addresses: Arc<OnceLock<HashSet<String>>>,
    jobs: Mutex<Option<Vec<Job>>>,
    shutdown: tokio::sync::Mutex<Vec<Job>>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        for job in self.jobs.get_mut().unwrap().iter().flatten() {
            job.cancel.send_replace(true);
        }
    }
}
struct Cancel(watch::Sender<bool>);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
#[derive(Clone)]
pub struct DeviceHostResolver(Arc<Shared>);
impl DeviceHostResolver {
    pub fn new(options: DeviceHostResolverOptions) -> Self {
        Self(Arc::new(Shared {
            options: Arc::new(options),
            addresses: Arc::new(OnceLock::new()),
            jobs: Mutex::new(Some(Vec::new())),
            shutdown: tokio::sync::Mutex::new(Vec::new()),
        }))
    }
    pub async fn is_local(&self, host: &SshDeviceHostConfig) -> bool {
        let (reply, receive) = oneshot::channel();
        let (cancel, mut cancelled) = watch::channel(false);
        let _guard = Cancel(cancel.clone());
        let options = self.0.options.clone();
        let local_addresses_cache = self.0.addresses.clone();
        let source = NativeProcessTable::command(
            options.ssh_binary.clone(),
            ssh_arguments(host),
            "ssh",
            options.ssh_timeout,
            usize::MAX,
        );
        {
            let mut jobs = self.0.jobs.lock().unwrap();
            let Some(jobs) = jobs.as_mut() else {
                return false;
            };
            jobs.retain(|job| !job.handle.is_finished());
            // The worker owns the command through cancellation and actual child
            // reaping; dropping the requesting socket cannot release it early.
            jobs.push(Job { cancel, handle: tokio::spawn(async move {
                let read = async {
                    let Ok(output) = source.output().await else { return false; };
                    if output.exit_code != Some(0) || output.stdout_truncated { return false; }
                    let text = String::from_utf8_lossy(&output.stdout);
                    let Some(hostname) = resolved_hostname(&text) else { return false; };
                    let addresses = if is_ip(&hostname) { vec![hostname] } else {
                        tokio::time::timeout(options.dns_timeout, (options.lookup)(hostname)).await.unwrap_or_default()
                    };
                    let local = local_addresses_cache.get_or_init(|| (options.local_addresses)());
                    local_addresses(&addresses, local)
                };
                let local = tokio::select! { biased; _ = cancelled.wait_for(|value| *value) => false, local = read => local };
                source.shutdown().await;
                let _ = reply.send(local);
            }) });
        }
        receive.await.unwrap_or(false)
    }
    /// Ordered like Effect.filter with concurrency four, even when aliases
    /// complete in a different order.
    pub async fn remote_hosts(&self, hosts: Vec<SshDeviceHostConfig>) -> Vec<SshDeviceHostConfig> {
        let mut resolved: Vec<_> =
            stream::iter(hosts.into_iter().enumerate())
                .map(|(index, host)| async move {
                    (index, (!self.is_local(&host).await).then_some(host))
                })
                .buffer_unordered(4)
                .collect()
                .await;
        resolved.sort_by_key(|(index, _)| *index);
        resolved.into_iter().filter_map(|(_, host)| host).collect()
    }
    pub async fn shutdown(&self) {
        let mut shutdown = self.0.shutdown.lock().await;
        shutdown.extend(self.0.jobs.lock().unwrap().take().unwrap_or_default());
        for job in shutdown.iter() {
            job.cancel.send_replace(true);
        }
        while let Some(job) = shutdown.first_mut() {
            if let Err(error) = (&mut job.handle).await {
                tracing::warn!(%error, "device-host resolver worker failed");
            }
            shutdown.remove(0);
        }
    }
}
pub fn ssh_arguments(host: &SshDeviceHostConfig) -> Vec<String> {
    let mut args = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
    ];
    if let Some(port) = host.port.flatten() {
        args.extend(["-p".into(), port.0.to_string()]);
    }
    args.push("-G".into());
    if let Some(identity) = host
        .identity_file
        .as_ref()
        .and_then(|identity| identity.as_ref())
    {
        args.extend(["-i".into(), identity.as_str().into()]);
    }
    args.push(host.target.as_str().into());
    args
}
fn resolved_hostname(output: &str) -> Option<String> {
    let config: HashMap<_, _> = output
        .split('\n')
        .map(|line| {
            match line.find(' ') {
                Some(separator) => (&line[..separator], trim_wire_string(&line[separator + 1..])),
                // JS slice(0,-1)/slice(0) when indexOf returns -1.
                None => (
                    &line[..line
                        .char_indices()
                        .last()
                        .map(|(index, character)| {
                            // A remaining unpaired surrogate cannot match any
                            // ASCII SSH key. Keep this key unknown instead of
                            // incorrectly removing the complete astral char.
                            if character.len_utf16() == 2 {
                                line.len()
                            } else {
                                index
                            }
                        })
                        .unwrap_or(0)],
                    trim_wire_string(line),
                ),
            }
        })
        .collect();
    if config.get("port") != Some(&"22")
        || ["proxycommand", "proxyjump"]
            .iter()
            .any(|key| config.get(key).is_some_and(|value| *value != "none"))
    {
        return None;
    }
    let hostname = config.get("hostname")?;
    let hostname = hostname.strip_prefix('[').unwrap_or(hostname);
    let hostname = hostname.strip_suffix(']').unwrap_or(hostname);
    (!hostname.is_empty()).then(|| hostname.to_string())
}
fn is_ip(hostname: &str) -> bool {
    hostname.parse::<IpAddr>().is_ok()
        || hostname.split_once('%').is_some_and(|(address, zone)| {
            address.parse::<Ipv6Addr>().is_ok()
                && !zone.is_empty()
                && zone
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-.:".contains(&byte))
        })
}
fn local_addresses(addresses: &[String], local: &HashSet<String>) -> bool {
    !addresses.is_empty()
        && addresses.iter().all(|address| {
            local.contains(address) || address == "::1" || address.starts_with("127.")
        })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        os::unix::fs::PermissionsExt,
        sync::atomic::{AtomicUsize, Ordering},
    };
    fn host(target: &str) -> SshDeviceHostConfig {
        serde_json::from_value(
            json!({"id":format!("fixture-{target}"),"label":target,"target":target}),
        )
        .unwrap()
    }
    fn fixture(directory: &std::path::Path, body: &str) -> PathBuf {
        let path = directory.join("ssh.py");
        std::fs::write(&path, format!("#!/usr/bin/env python3\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    #[test]
    fn original_alias_classification_and_ip_literal_witnesses() {
        for line in include_str!("../tests/fixtures/device-host-resolver.jsonl").lines() {
            let row: Value = serde_json::from_str(line).unwrap();
            if row["op"] == "ip" {
                assert_eq!(
                    is_ip(row["hostname"].as_str().unwrap()),
                    row["result"].as_bool().unwrap(),
                    "{row}"
                );
                continue;
            }
            let local = serde_json::from_value::<Vec<String>>(row["local"].clone())
                .unwrap()
                .into_iter()
                .collect();
            let actual = resolved_hostname(row["stdout"].as_str().unwrap())
                .map(|hostname| {
                    if is_ip(&hostname) {
                        local_addresses(&[hostname], &local)
                    } else {
                        false
                    }
                })
                .unwrap_or(false);
            assert_eq!(actual, row["result"].as_bool().unwrap(), "{row}");
        }
    }
    #[tokio::test]
    async fn actual_ssh_arguments_stable_filter_and_lazy_address_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fixture(
            directory.path(),
            "import sys,json\na=sys.argv[1:]\nassert a[:4]==['-o','BatchMode=yes','-o','ConnectTimeout=10']\nassert '-G' in a\nt=a[-1]\nif t=='identity': assert a[4:]==['-p','2222','-G','-i','key with spaces','identity']\nprint('hostname '+{'local':'100.65.180.100','loopback':'127.0.1.1','ipv6':'::1','dns':'alias.test'}.get(t,'192.0.2.1'))\nprint('port 22')\nif t=='proxy': print('proxyjump bastion')\nif t=='failed': sys.exit(4)",
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let resolver = DeviceHostResolver::new(DeviceHostResolverOptions {
            ssh_binary: binary,
            local_addresses: Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                HashSet::from(["100.65.180.100".into()])
            }),
            lookup: Arc::new(|hostname| {
                Box::pin(async move {
                    assert_eq!(hostname, "alias.test");
                    vec!["127.0.0.1".into(), "192.0.2.1".into()]
                })
            }),
            ..Default::default()
        });
        assert!(!resolver.is_local(&host("proxy")).await);
        assert!(!resolver.is_local(&host("failed")).await);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut identity = host("identity");
        identity.port = Some(Some(t3_contracts::base::RangeInt(2222)));
        identity.identity_file = Some(Some(
            t3_contracts::TrimmedNonEmptyString::new("key with spaces").unwrap(),
        ));
        assert!(!resolver.is_local(&identity).await);
        let targets = ["local", "remote", "loopback", "dns", "ipv6", "proxy"];
        let result = resolver
            .remote_hosts(targets.iter().map(|target| host(target)).collect())
            .await;
        assert_eq!(
            result
                .iter()
                .map(|host| host.target.as_str())
                .collect::<Vec<_>>(),
            ["remote", "dns", "proxy"]
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        resolver.shutdown().await;
        assert!(!resolver.is_local(&host("local")).await);
    }
    #[tokio::test]
    async fn caller_cancel_and_shutdown_reap_owned_ssh_child() {
        use tokio::io::AsyncBufReadExt;
        let directory = tempfile::tempdir().unwrap();
        let listener =
            tokio::net::UnixListener::bind(directory.path().join("started.sock")).unwrap();
        let body = format!(
            "import os,socket\ns=socket.socket(socket.AF_UNIX);s.connect({:?});s.sendall((str(os.getpid())+'\\n').encode());s.recv(1)",
            directory.path().join("started.sock").to_string_lossy()
        );
        let resolver = DeviceHostResolver::new(DeviceHostResolverOptions {
            ssh_binary: fixture(directory.path(), &body),
            ..Default::default()
        });
        let active = resolver.clone();
        let request = tokio::spawn(async move { active.is_local(&host("held")).await });
        let (socket, _) = listener.accept().await.unwrap();
        let mut line = String::new();
        let mut socket = tokio::io::BufReader::new(socket);
        socket.read_line(&mut line).await.unwrap();
        let pid: i32 = line.trim().parse().unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        resolver.shutdown().await;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn dns_timeout_is_remote_and_shutdown_cancels_held_lookup() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fixture(directory.path(), "print('hostname alias.test\\nport 22')");
        let entered = Arc::new(tokio::sync::Notify::new());
        let signal = entered.clone();
        let resolver = DeviceHostResolver::new(DeviceHostResolverOptions {
            ssh_binary: binary,
            lookup: Arc::new(move |_| {
                let signal = signal.clone();
                Box::pin(async move {
                    signal.notify_one();
                    std::future::pending().await
                })
            }),
            ..Default::default()
        });
        let active = resolver.clone();
        let request = tokio::spawn(async move { active.is_local(&host("dns")).await });
        entered.notified().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(!request.await.unwrap());
        tokio::time::resume();
        let active = resolver.clone();
        let request = tokio::spawn(async move { active.is_local(&host("dns")).await });
        entered.notified().await;
        resolver.shutdown().await;
        assert!(!request.await.unwrap());
    }

    #[tokio::test]
    async fn fanout_admits_four_and_preserves_input_order_when_dns_finishes_out_of_order() {
        let directory = tempfile::tempdir().unwrap();
        let binary = fixture(
            directory.path(),
            "import sys\nprint('hostname '+sys.argv[-1]+'.test\\nport 22')",
        );
        let (entered, mut started) = tokio::sync::mpsc::unbounded_channel();
        let resolver = DeviceHostResolver::new(DeviceHostResolverOptions {
            ssh_binary: binary,
            lookup: Arc::new(move |hostname| {
                let (release, wait) = oneshot::channel();
                entered.send((hostname, release)).unwrap();
                Box::pin(async move {
                    wait.await.unwrap();
                    vec!["192.0.2.1".into()]
                })
            }),
            ..Default::default()
        });
        let active = resolver.clone();
        let request = tokio::spawn(async move {
            active
                .remote_hosts((0..5).map(|n| host(&format!("dns{n}"))).collect())
                .await
        });
        let mut releases = HashMap::new();
        for _ in 0..4 {
            let (hostname, release) = started.recv().await.unwrap();
            releases.insert(hostname, release);
        }
        assert!(started.try_recv().is_err());
        // Any completed lookup releases fanout capacity; a slow first alias
        // does not prevent the fifth from starting. Result order stays stable.
        releases.remove("dns3.test").unwrap().send(()).unwrap();
        let (hostname, release) = started.recv().await.unwrap();
        assert_eq!(hostname, "dns4.test");
        release.send(()).unwrap();
        for n in (0..3).rev() {
            releases
                .remove(&format!("dns{n}.test"))
                .unwrap()
                .send(())
                .unwrap();
        }
        let result = request.await.unwrap();
        assert_eq!(
            result
                .iter()
                .map(|host| host.target.as_str())
                .collect::<Vec<_>>(),
            ["dns0", "dns1", "dns2", "dns3", "dns4"]
        );
        resolver.shutdown().await;
    }
}
