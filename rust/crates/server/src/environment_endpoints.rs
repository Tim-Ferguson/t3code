//! The `directEndpoints` and `remoteOpenTargets` of the server config (`environment/DirectEndpoints.ts`,
//! `environment/RemoteOpenTargets.ts`).
//!
//! Direct endpoints are the plain HTTP addresses of this machine's private LAN and tailnet IPv4 interfaces that the
//! listener accepts on (none for a loopback-only server, whose loopback address means a different machine to every
//! client), plus the tailnet HTTPS name when Tailscale Serve is on and the name answers as this server. Remote open
//! targets are the SSH names an editor deep link can use: the tailnet name and `<hostname>.local`, only when an sshd
//! listens on this machine. Both are discovery: they degrade to an empty list after five seconds, as the original's config
//! read does, and the tailnet probe is cut at three so the LAN addresses are never held back by it.
//!
//! The tailnet is reached through [`Tailnet`], which the server fills with the `tailscale` CLI of `t3-remote` and a test
//! fills with a fake: the tests never run `tailscale`.
use crate::persistence::Store;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{net::IpAddr, sync::Arc, time::Duration};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const TAILSCALE_ENDPOINT_TIMEOUT: Duration = Duration::from_secs(3);
const SSH_PORT: u16 = 22;
const LISTENER_TIMEOUT: Duration = Duration::from_millis(250);

/// A network interface address.
#[derive(Debug, Clone)]
pub struct Iface {
    pub name: String,
    pub address: IpAddr,
    pub internal: bool,
}

/// What the endpoints ask of the tailnet.
#[async_trait]
pub trait Tailnet: Send + Sync + 'static {
    /// The MagicDNS name of this machine, when Tailscale is up. Absent or down is the common case, not an error.
    async fn magic_dns_name(&self) -> Option<String>;
    /// Whether the HTTPS endpoint answers as this server.
    async fn answers(&self, base_url: &str) -> bool;
}

/// The `tailscale` CLI.
pub struct CliTailnet(pub t3_remote::tailscale::Tailscale);

#[async_trait]
impl Tailnet for CliTailnet {
    async fn magic_dns_name(&self) -> Option<String> {
        self.0.read_status().await.ok().and_then(|status| status.magic_dns_name)
    }
    async fn answers(&self, base_url: &str) -> bool {
        t3_remote::tailscale::probe_tailscale_https_endpoint(base_url, None).await
    }
}

type Interfaces = Arc<dyn Fn() -> Vec<Iface> + Send + Sync>;
type Listener = Arc<dyn Fn(IpAddr, u16) -> futures_util::future::BoxFuture<'static, bool> + Send + Sync>;

pub struct Endpoints {
    /// The address the listener is bound to, as configured.
    pub host: String,
    pub port: u16,
    /// The HTTPS port of Tailscale Serve when it is switched on.
    pub tailscale_serve: Option<u16>,
    pub interfaces: Interfaces,
    pub tailnet: Arc<dyn Tailnet>,
    pub listener: Listener,
    pub hostname: Arc<dyn Fn() -> Option<String> + Send + Sync>,
}

impl Endpoints {
    /// The process's: the machine's interfaces, the `tailscale` CLI, a connect probe for sshd and the machine's hostname.
    pub fn production(host: String, port: u16, tailscale_serve: Option<u16>) -> Self {
        Self {
            host,
            port,
            tailscale_serve,
            interfaces: Arc::new(|| {
                if_addrs::get_if_addrs()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|interface| Iface { name: interface.name.clone(), address: interface.ip(), internal: interface.is_loopback() })
                    .collect()
            }),
            tailnet: Arc::new(CliTailnet(t3_remote::tailscale::Tailscale::new(cfg!(windows)))),
            listener: Arc::new(|address, port| Box::pin(has_listener(address, port))),
            hostname: Arc::new(machine_hostname),
        }
    }

    /// `DirectEndpoints.resolve`, cut at the config discovery timeout.
    pub async fn direct(&self) -> Vec<Value> {
        tokio::time::timeout(DISCOVERY_TIMEOUT, self.resolve_direct()).await.unwrap_or_default()
    }

    async fn resolve_direct(&self) -> Vec<Value> {
        let interfaces = (self.interfaces)();
        let mut endpoints = resolve_bound_endpoints(&self.host, self.port, &interfaces);
        // Tailscale Serve terminates HTTPS on the tailnet name and forwards to loopback, so it works even for a loopback-only
        // server. The name is listed only once it answers as this server: Serve setup can fail without stopping startup.
        if let Some(serve_port) = self.tailscale_serve {
            let served = tokio::time::timeout(TAILSCALE_ENDPOINT_TIMEOUT, async {
                let name = self.tailnet.magic_dns_name().await?;
                let base_url = t3_remote::tailscale::build_tailscale_https_base_url(&name, Some(serve_port)).ok()?;
                self.tailnet.answers(&base_url).await.then_some(base_url)
            })
            .await
            .ok()
            .flatten();
            if let Some(base_url) = served {
                endpoints.push(json!({"kind": "tailnet", "httpBaseUrl": base_url}));
            }
        }
        endpoints
    }

    /// `RemoteOpenTargets.resolveTargets`, cut at the config discovery timeout.
    pub async fn remote_open_targets(&self) -> Vec<Value> {
        tokio::time::timeout(DISCOVERY_TIMEOUT, self.resolve_targets()).await.unwrap_or_default()
    }

    async fn resolve_targets(&self) -> Vec<Value> {
        // No local sshd means no name can work; advertise nothing so clients render a clear "no SSH route" state instead of
        // links that hang. Both loopback families are checked: sshd can be bound IPv6-only.
        let (ipv4, ipv6) = tokio::join!((self.listener)(IpAddr::from([127, 0, 0, 1]), SSH_PORT), (self.listener)(IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]), SSH_PORT));
        if !(ipv4 || ipv6) {
            return vec![];
        }
        let mut targets = vec![];
        if let Some(name) = self.tailnet.magic_dns_name().await {
            targets.push(json!({"kind": "tailscale", "host": name}));
        }
        // `os.hostname()` may already be an FQDN (macOS often reports "Name.local"); mDNS names are always `<first label>.local`.
        if let Some(hostname) = (self.hostname)() {
            let short = hostname.split('.').next().unwrap_or_default().trim();
            if !short.is_empty() {
                targets.push(json!({"kind": "mdns", "host": format!("{short}.local")}));
            }
        }
        targets
    }
}

async fn has_listener(address: IpAddr, port: u16) -> bool {
    matches!(tokio::time::timeout(LISTENER_TIMEOUT, tokio::net::TcpStream::connect((address, port))).await, Ok(Ok(_)))
}

fn machine_hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is writable for its whole length, and `gethostname` NUL-terminates within it or fails.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

// ---- DirectEndpoints.ts ---------------------------------------------------------------------------------------------

/// `isLoopbackHost` (`startupAccess.ts`).
fn is_loopback_host(host: &str) -> bool {
    host.is_empty() || matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.starts_with("127.")
}

fn is_wildcard_host(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]")
}

fn format_host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_owned() }
}

/// `isPrivateIpv4Address` of `hostClassification.ts`.
fn is_private_ipv4(octets: [u8; 4]) -> bool {
    let [a, b, ..] = octets;
    a == 0 || a == 10 || a == 127 || (a == 100 && (64..=127).contains(&b)) || (a == 172 && (16..=31).contains(&b)) || (a == 192 && b == 168) || (a == 169 && b == 254) || (a == 198 && (18..=19).contains(&b))
}

/// Only numeric private-network and tailnet IPv4 addresses are reported. These routes are plain HTTP and carry the client's
/// credential, so a public address would send it across the internet unencrypted, and a name (`server.local`) can resolve to
/// a different machine on each client's network.
fn is_advertisable_address(address: &str) -> bool {
    let Ok(parsed) = address.parse::<std::net::Ipv4Addr>() else { return false };
    // `net.isIPv4` accepts only the canonical dotted form, which `Ipv4Addr` parsing does too.
    !address.starts_with("127.") && !address.starts_with("169.254.") && (t3_remote::tailscale::is_tailscale_ipv4_address(address) || is_private_ipv4(parsed.octets()))
}

/// Container and VM networks have private addresses only this machine reaches. A host bridge such as `br0` or Proxmox's
/// `vmbr0` carries the LAN address, so it stays listed.
fn is_virtual_interface(name: &str) -> bool {
    ["docker", "br-", "veth", "virbr", "vmnet", "vboxnet", "vEthernet", "podman", "cni", "flannel", "cali", "lxcbr", "lxdbr"].iter().any(|prefix| name.starts_with(prefix))
        || name.strip_prefix("bridge1").is_some_and(|rest| rest.len() == 2 && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// `resolveBoundEndpoints`: plain HTTP endpoints for the private addresses a server bound to `host` accepts. IPv4 only.
pub fn resolve_bound_endpoints(host: &str, port: u16, interfaces: &[Iface]) -> Vec<Value> {
    if is_loopback_host(host) {
        return vec![];
    }
    let addresses: Vec<String> = if is_wildcard_host(host) {
        interfaces
            .iter()
            .filter(|interface| !is_virtual_interface(&interface.name))
            .filter(|interface| !interface.internal && interface.address.is_ipv4() && is_advertisable_address(&interface.address.to_string()))
            .map(|interface| interface.address.to_string())
            .collect()
    } else {
        [host.to_owned()].into_iter().filter(|host| is_advertisable_address(host)).collect()
    };
    let mut seen = std::collections::HashSet::new();
    addresses
        .into_iter()
        .filter(|address| seen.insert(address.clone()))
        .map(|address| json!({"kind": if t3_remote::tailscale::is_tailscale_ipv4_address(&address) { "tailnet" } else { "lan" }, "httpBaseUrl": format!("http://{}:{port}/", format_host_for_url(&address))}))
        .collect()
}

// ---- wiring -----------------------------------------------------------------------------------------------------------

/// Makes `endpoints` the ones `server.getConfig` and the config subscription report.
pub fn install(store: &Store, endpoints: Endpoints) {
    *store.extension(Slot::default).0.lock().unwrap() = Some(Arc::new(endpoints));
}

#[derive(Default)]
struct Slot(std::sync::Mutex<Option<Arc<Endpoints>>>);

/// The `directEndpoints` and `remoteOpenTargets` of a config snapshot. A store with no endpoints installed leaves the
/// snapshot's empty lists.
pub async fn overlay_config(store: &Store, config: &mut Value) {
    let Some(endpoints) = store.extension(Slot::default).0.lock().unwrap().clone() else { return };
    let (direct, targets) = tokio::join!(endpoints.direct(), endpoints.remote_open_targets());
    if let Some(object) = config.as_object_mut() {
        object.insert("directEndpoints".into(), Value::Array(direct));
        object.insert("remoteOpenTargets".into(), Value::Array(targets));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iface(name: &str, address: &str, internal: bool) -> Iface {
        Iface { name: name.into(), address: address.parse().unwrap(), internal }
    }

    #[test]
    fn a_loopback_server_lists_nothing_and_a_wildcard_server_lists_its_private_and_tailnet_addresses() {
        let interfaces = [
            iface("lo0", "127.0.0.1", true),
            iface("en0", "192.168.1.44", false),
            iface("en0", "fe80::1", false),
            iface("en1", "8.8.4.4", false),
            iface("utun3", "100.100.100.100", false),
            iface("docker0", "172.17.0.1", false),
            iface("br0", "10.0.0.7", false),
            iface("bridge102", "192.168.64.1", false),
            iface("bridge2", "192.168.65.1", false),
            iface("en2", "169.254.3.4", false),
            iface("en3", "192.168.1.44", false),
            iface("en4", "172.32.0.1", false),
        ];
        let urls = |host: &str| resolve_bound_endpoints(host, 3773, &interfaces).iter().map(|endpoint| format!("{} {}", endpoint["kind"].as_str().unwrap(), endpoint["httpBaseUrl"].as_str().unwrap())).collect::<Vec<_>>();
        assert!(urls("127.0.0.1").is_empty() && urls("localhost").is_empty() && urls("").is_empty() && urls("::1").is_empty());
        assert_eq!(urls("0.0.0.0"), ["lan http://192.168.1.44:3773/", "tailnet http://100.100.100.100:3773/", "lan http://10.0.0.7:3773/", "lan http://192.168.65.1:3773/"], "no loopback, link-local, public, IPv6 or virtual-network address, and each once");
        assert_eq!(urls("::"), urls("0.0.0.0"));
        assert_eq!(urls("192.168.7.7"), ["lan http://192.168.7.7:3773/"]);
        assert_eq!(urls("100.64.0.9"), ["tailnet http://100.64.0.9:3773/"]);
        assert!(urls("8.8.8.8").is_empty() && urls("server.local").is_empty() && urls("fd00::1").is_empty() && urls("010.0.0.1").is_empty());
    }

    struct FakeTailnet {
        name: Option<&'static str>,
        answers: bool,
        asked: std::sync::Mutex<Vec<String>>,
    }
    #[async_trait]
    impl Tailnet for FakeTailnet {
        async fn magic_dns_name(&self) -> Option<String> {
            self.name.map(str::to_owned)
        }
        async fn answers(&self, base_url: &str) -> bool {
            self.asked.lock().unwrap().push(base_url.to_owned());
            self.answers
        }
    }

    fn endpoints(host: &str, serve: Option<u16>, tailnet: Arc<FakeTailnet>, sshd: bool, hostname: Option<&'static str>) -> Endpoints {
        Endpoints {
            host: host.into(),
            port: 3773,
            tailscale_serve: serve,
            interfaces: Arc::new(|| vec![iface("en0", "192.168.1.44", false)]),
            tailnet,
            listener: Arc::new(move |_, port| {
                assert_eq!(port, SSH_PORT);
                Box::pin(async move { sshd })
            }),
            hostname: Arc::new(move || hostname.map(str::to_owned)),
        }
    }

    #[tokio::test]
    async fn the_tailnet_name_is_listed_only_when_serve_is_on_and_it_answers_as_this_server() {
        let tailnet = |answers| Arc::new(FakeTailnet { name: Some("desktop.tail.ts.net"), answers, asked: Default::default() });
        // A loopback-only server still lists its served name, and not before it answers.
        let answering = tailnet(true);
        let direct = endpoints("127.0.0.1", Some(8443), answering.clone(), false, None).direct().await;
        assert_eq!(direct, [json!({"kind": "tailnet", "httpBaseUrl": "https://desktop.tail.ts.net:8443/"})]);
        assert_eq!(*answering.asked.lock().unwrap(), ["https://desktop.tail.ts.net:8443/"]);
        let default_port = endpoints("127.0.0.1", Some(443), tailnet(true), false, None).direct().await;
        assert_eq!(default_port[0]["httpBaseUrl"], "https://desktop.tail.ts.net/");
        assert!(endpoints("127.0.0.1", Some(8443), tailnet(false), false, None).direct().await.is_empty(), "a name that does not answer is left out");
        let silent = tailnet(true);
        assert!(endpoints("127.0.0.1", None, silent.clone(), false, None).direct().await.is_empty());
        assert!(silent.asked.lock().unwrap().is_empty(), "serve off: the tailnet is not asked");
        let nameless = Arc::new(FakeTailnet { name: None, answers: true, asked: Default::default() });
        assert!(endpoints("127.0.0.1", Some(443), nameless, false, None).direct().await.is_empty());
        // The LAN addresses of a wildcard server come first and are kept when the tailnet name is not.
        let lan = endpoints("0.0.0.0", Some(443), tailnet(false), false, None).direct().await;
        assert_eq!(lan, [json!({"kind": "lan", "httpBaseUrl": "http://192.168.1.44:3773/"})]);
    }

    #[tokio::test]
    async fn ssh_names_are_offered_only_when_an_sshd_listens_here() {
        let tailnet = Arc::new(FakeTailnet { name: Some("desktop.tail.ts.net"), answers: true, asked: Default::default() });
        let none = endpoints("0.0.0.0", None, tailnet.clone(), false, Some("Mac.local")).remote_open_targets().await;
        assert!(none.is_empty(), "no sshd, no route");
        let both = endpoints("0.0.0.0", None, tailnet, true, Some("Mac.local")).remote_open_targets().await;
        assert_eq!(both, [json!({"kind": "tailscale", "host": "desktop.tail.ts.net"}), json!({"kind": "mdns", "host": "Mac.local"})]);
        let bare = Arc::new(FakeTailnet { name: None, answers: false, asked: Default::default() });
        let host_only = endpoints("0.0.0.0", None, bare.clone(), true, Some("workstation.example.com")).remote_open_targets().await;
        assert_eq!(host_only, [json!({"kind": "mdns", "host": "workstation.local"})], "the first label of an FQDN");
        assert!(endpoints("0.0.0.0", None, bare.clone(), true, None).remote_open_targets().await.is_empty());
        assert!(endpoints("0.0.0.0", None, bare, true, Some("  ")).remote_open_targets().await.is_empty());
    }

    #[tokio::test]
    async fn the_overlay_fills_a_config_with_values_of_the_contract_shapes_and_leaves_one_without_endpoints_alone() {
        let store = Store::memory().unwrap();
        let mut config = json!({"directEndpoints": [], "remoteOpenTargets": []});
        overlay_config(&store, &mut config).await;
        assert_eq!(config, json!({"directEndpoints": [], "remoteOpenTargets": []}));
        let tailnet = Arc::new(FakeTailnet { name: Some("desktop.tail.ts.net"), answers: true, asked: Default::default() });
        install(&store, endpoints("0.0.0.0", Some(443), tailnet, true, Some("Mac")));
        overlay_config(&store, &mut config).await;
        for endpoint in config["directEndpoints"].as_array().unwrap() {
            serde_json::from_value::<t3_contracts::ServerDirectEndpoint>(endpoint.clone()).unwrap();
        }
        for target in config["remoteOpenTargets"].as_array().unwrap() {
            serde_json::from_value::<t3_contracts::RemoteOpenTarget>(target.clone()).unwrap();
        }
        assert_eq!((config["directEndpoints"].as_array().unwrap().len(), config["remoteOpenTargets"].as_array().unwrap().len()), (2, 2));
    }
}
