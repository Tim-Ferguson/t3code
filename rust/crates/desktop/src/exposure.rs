//! Whether the server this app started listens on this machine only or on the local network, and which addresses a pairing link can
//! name: the running side of `network` (the pure rules) and `settings` (what the person chose). Port of `DesktopServerExposure.ts`.
//! A change is saved, written into the `Binding` the server's next start reads, and reported with whether that start is needed: the
//! caller restarts the server (the original relaunches the whole app). Nothing here runs a program or binds a socket: the machine's
//! addresses and Tailscale come through the `Machine` and `Tailscale` traits, so a test supplies its own.
use crate::{
    network::{self, Interface, Mode},
    settings,
};
use async_trait::async_trait;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use t3_client::access::Endpoint;

/// The addresses of this machine's network interfaces (`DesktopNetworkInterfaces`).
pub trait Machine: Send + Sync {
    fn interfaces(&self) -> Vec<Interface>;
}

/// The interfaces the system reports.
pub struct SystemMachine;
impl Machine for SystemMachine {
    fn interfaces(&self) -> Vec<Interface> {
        if_addrs::get_if_addrs().map(|found| found.into_iter().map(|interface| Interface { address: interface.ip().to_string(), ipv4: interface.ip().is_ipv4(), internal: interface.is_loopback() }).collect()).unwrap_or_default()
    }
}

/// What Tailscale says about this machine: the program is asked only for the MagicDNS name, and the HTTPS address it would serve is probed.
#[async_trait]
pub trait Tailscale: Send + Sync {
    /// The machine's MagicDNS name; none when Tailscale is absent, stopped or does not answer.
    async fn magic_dns_name(&self) -> Option<String>;
    /// Whether `base_url` answers the environment descriptor.
    async fn answers(&self, base_url: &str) -> bool;
}

/// How long the answer of `tailscale status` is kept (`TAILSCALE_STATUS_CACHE_TTL`): on macOS each ask of the program from the Mac App
/// Store's Tailscale makes the system prompt for permission again.
const STATUS_CACHE: Duration = Duration::from_secs(60);

/// The `tailscale` program, run through `t3_remote::tailscale`.
pub struct SystemTailscale {
    program: t3_remote::tailscale::Tailscale,
    cached: Mutex<Option<(Instant, Option<String>)>>,
}
impl SystemTailscale {
    pub fn new() -> Self {
        Self { program: t3_remote::tailscale::Tailscale::new(cfg!(windows)), cached: Mutex::new(None) }
    }
}
impl Default for SystemTailscale {
    fn default() -> Self {
        Self::new()
    }
}
#[async_trait]
impl Tailscale for SystemTailscale {
    async fn magic_dns_name(&self) -> Option<String> {
        if let Some((at, name)) = &*self.cached.lock().expect("the status cache") {
            if at.elapsed() < STATUS_CACHE {
                return name.clone();
            }
        }
        let name = self.program.read_status().await.ok().and_then(|status| status.magic_dns_name);
        *self.cached.lock().expect("the status cache") = Some((Instant::now(), name.clone()));
        name
    }
    async fn answers(&self, base_url: &str) -> bool {
        t3_remote::tailscale::probe_tailscale_https_endpoint(base_url, None).await
    }
}

/// How the server's next start is set up, shared with the launcher (`ServerLauncher::with_binding`).
#[derive(Debug)]
pub struct Binding(Mutex<Bound>);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bound {
    pub host: String,
    /// The HTTPS port Tailscale Serve is asked for, when it is on.
    pub tailscale_serve_port: Option<u16>,
}
impl Binding {
    pub fn new(bound: Bound) -> Arc<Self> {
        Arc::new(Self(Mutex::new(bound)))
    }
    pub fn get(&self) -> Bound {
        self.0.lock().expect("the binding").clone()
    }
    pub(crate) fn set(&self, bound: Bound) {
        *self.0.lock().expect("the binding") = bound;
    }
}

/// `DesktopServerExposureState`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub mode: Mode,
    pub endpoint_url: Option<String>,
    pub advertised_host: Option<String>,
    pub tailscale_serve_enabled: bool,
    pub tailscale_serve_port: u16,
}

/// `DesktopServerExposureChange`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub state: State,
    /// The server has to start again for the change to show.
    pub requires_relaunch: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("No reachable network address is available for desktop network access on port {port}.")]
    NoNetworkAddress { port: u16 },
    #[error("Failed to persist desktop server exposure mode {mode}.")]
    Persist { mode: &'static str, #[source] cause: std::io::Error },
    #[error("Failed to persist desktop Tailscale Serve settings (enabled: {enabled}, port: {port}).")]
    PersistTailscale { enabled: bool, port: String, #[source] cause: std::io::Error },
}

struct Runtime {
    exposure: network::Exposure,
    tailscale_serve_enabled: bool,
    tailscale_serve_port: u16,
}

pub struct Exposure {
    settings: PathBuf,
    port: u16,
    machine: Arc<dyn Machine>,
    /// `T3CODE_DESKTOP_LAN_HOST`.
    host_override: Option<String>,
    /// `T3CODE_DESKTOP_HTTPS_ENDPOINTS`, split.
    custom_https: Vec<String>,
    binding: Arc<Binding>,
    /// Each change reads this, saves the settings, then writes it back; one at a time, so that a change never writes over another
    /// with values it read before that one saved.
    runtime: Mutex<Runtime>,
}

/// The mode asked for, the exposure it gives on this machine, and whether it cannot be had: network access with no address to reach
/// this machine at (no outward IPv4 address and no Tailscale one) falls back to this machine only.
fn resolve(requested: Mode, port: u16, interfaces: &[Interface], host_override: Option<&str>) -> (network::Exposure, bool) {
    let requested_exposure = network::resolve_exposure(requested, port, interfaces, host_override);
    let has_tailscale = interfaces.iter().any(|interface| !interface.internal && interface.ipv4 && t3_remote::tailscale::is_tailscale_ipv4_address(&interface.address));
    let unavailable = requested == Mode::NetworkAccessible && requested_exposure.endpoint_url.is_none() && !has_tailscale;
    if unavailable { (network::resolve_exposure(Mode::LocalOnly, port, interfaces, host_override), true) } else { (requested_exposure, false) }
}

fn serve_port(settings: &settings::Settings) -> Option<u16> {
    settings.tailscale_serve_enabled.then_some(settings.tailscale_serve_port)
}

/// The HTTPS addresses the person set, from a comma separated list.
pub fn split_endpoints(list: &str) -> Vec<String> {
    list.split(',').map(str::trim).filter(|url| !url.is_empty()).map(str::to_owned).collect()
}

impl Exposure {
    /// `configureFromSettings`: the exposure the saved settings ask for, on the port the server was given.
    pub fn configure(settings_path: PathBuf, port: u16, machine: Arc<dyn Machine>, host_override: Option<String>, custom_https: Vec<String>) -> Self {
        let saved = settings::load(&settings_path);
        let host_override = host_override.map(|host| host.trim().to_owned()).filter(|host| !host.is_empty());
        let (exposure, _) = resolve(saved.server_exposure_mode, port, &machine.interfaces(), host_override.as_deref());
        let binding = Binding::new(Bound { host: exposure.bind_host.clone(), tailscale_serve_port: serve_port(&saved) });
        let runtime = Runtime { exposure, tailscale_serve_enabled: saved.tailscale_serve_enabled, tailscale_serve_port: saved.tailscale_serve_port };
        Self { settings: settings_path, port, machine, host_override, custom_https, binding, runtime: Mutex::new(runtime) }
    }

    /// What the server's next start reads.
    pub fn binding(&self) -> Arc<Binding> {
        self.binding.clone()
    }

    fn state_of(runtime: &Runtime) -> State {
        State { mode: runtime.exposure.mode, endpoint_url: runtime.exposure.endpoint_url.clone(), advertised_host: runtime.exposure.advertised_host.clone(), tailscale_serve_enabled: runtime.tailscale_serve_enabled, tailscale_serve_port: runtime.tailscale_serve_port }
    }

    pub fn state(&self) -> State {
        Self::state_of(&self.runtime.lock().expect("the exposure"))
    }

    /// `setMode`. Refused, with nothing saved, when network access has no address to be reached at.
    pub fn set_mode(&self, mode: Mode) -> Result<Change, Error> {
        let mut runtime = self.runtime.lock().expect("the exposure");
        let (exposure, unavailable) = resolve(mode, self.port, &self.machine.interfaces(), self.host_override.as_deref());
        if unavailable {
            return Err(Error::NoNetworkAddress { port: self.port });
        }
        let saved = settings::update(&self.settings, |settings| settings.server_exposure_mode = mode).map_err(|cause| Error::Persist { mode: mode.wire(), cause })?;
        // `requiresBackendRelaunch`: the port is the same for the life of the app, so the bind address and the local address decide.
        let requires_relaunch = saved.changed || exposure.bind_host != runtime.exposure.bind_host || exposure.local_http_url != runtime.exposure.local_http_url;
        runtime.exposure = exposure;
        self.binding.set(Bound { host: runtime.exposure.bind_host.clone(), tailscale_serve_port: serve_port(&saved.settings) });
        Ok(Change { state: Self::state_of(&runtime), requires_relaunch })
    }

    /// `setTailscaleServeEnabled`. A port out of range is the default one; none keeps the saved port.
    pub fn set_tailscale_serve(&self, enabled: bool, port: Option<u16>) -> Result<Change, Error> {
        let mut runtime = self.runtime.lock().expect("the exposure");
        let saved = settings::update(&self.settings, |settings| {
            settings.tailscale_serve_enabled = enabled;
            if let Some(port) = port {
                settings.tailscale_serve_port = if port >= 1 { port } else { settings::DEFAULT_TAILSCALE_SERVE_PORT };
            }
        })
        .map_err(|cause| Error::PersistTailscale { enabled, port: port.map_or_else(|| "unchanged".to_owned(), |port| port.to_string()), cause })?;
        runtime.tailscale_serve_enabled = saved.settings.tailscale_serve_enabled;
        runtime.tailscale_serve_port = saved.settings.tailscale_serve_port;
        self.binding.set(Bound { host: runtime.exposure.bind_host.clone(), tailscale_serve_port: serve_port(&saved.settings) });
        Ok(Change { state: Self::state_of(&runtime), requires_relaunch: saved.changed })
    }

    /// `getAdvertisedEndpoints`: this machine, the local network when it is open, the addresses the person set, and, when the network is
    /// open or Serve is on, Tailscale's. The program is not asked otherwise.
    pub async fn advertised_endpoints(&self, tailscale: &dyn Tailscale) -> Vec<Endpoint> {
        let (exposure, serve_enabled, port) = {
            let runtime = self.runtime.lock().expect("the exposure");
            (runtime.exposure.clone(), runtime.tailscale_serve_enabled, runtime.tailscale_serve_port)
        };
        let mut endpoints = network::core_endpoints(self.port, &exposure, &self.custom_https);
        if !network::should_ask_tailscale(exposure.mode, serve_enabled) {
            return endpoints;
        }
        endpoints.extend(network::tailscale_ip_endpoints(self.port, &self.machine.interfaces()));
        if let Some(name) = tailscale.magic_dns_name().await {
            let reachable = match serve_enabled.then(|| t3_remote::tailscale::build_tailscale_https_base_url(&name, Some(port)).ok()).flatten() {
                Some(base) => tailscale.answers(&base).await,
                None => false,
            };
            endpoints.extend(network::tailscale_magic_dns_endpoint(Some(&name), serve_enabled, Some(port), reachable));
        }
        endpoints
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use t3_client::access::Reachability;

    struct Fixed(Vec<Interface>);
    impl Machine for Fixed {
        fn interfaces(&self) -> Vec<Interface> {
            self.0.clone()
        }
    }
    fn interface(address: &str, ipv4: bool, internal: bool) -> Interface {
        Interface { address: address.into(), ipv4, internal }
    }
    fn wired() -> Arc<dyn Machine> {
        Arc::new(Fixed(vec![interface("127.0.0.1", true, true), interface("192.168.1.20", true, false)]))
    }
    fn tailnet_only() -> Arc<dyn Machine> {
        Arc::new(Fixed(vec![interface("127.0.0.1", true, true), interface("100.101.102.103", true, false)]))
    }
    fn offline() -> Arc<dyn Machine> {
        Arc::new(Fixed(vec![interface("127.0.0.1", true, true)]))
    }
    fn fixture(machine: Arc<dyn Machine>) -> (tempfile::TempDir, Exposure) {
        let directory = tempfile::Builder::new().prefix("t3code-rust-port-test-desktop-exposure-").tempdir().unwrap();
        let exposure = Exposure::configure(directory.path().join("desktop-settings.json"), 3774, machine, None, vec!["https://t3.example.test".into()]);
        (directory, exposure)
    }

    /// Tailscale as a test says it is; the real program is never run.
    struct FakeTailscale {
        name: Option<&'static str>,
        answers: bool,
        asked: Mutex<Vec<&'static str>>,
    }
    #[async_trait]
    impl Tailscale for FakeTailscale {
        async fn magic_dns_name(&self) -> Option<String> {
            self.asked.lock().unwrap().push("status");
            self.name.map(str::to_owned)
        }
        async fn answers(&self, _base_url: &str) -> bool {
            self.asked.lock().unwrap().push("probe");
            self.answers
        }
    }
    fn tailscale(name: Option<&'static str>, answers: bool) -> FakeTailscale {
        FakeTailscale { name, answers, asked: Mutex::new(Vec::new()) }
    }

    #[test]
    fn the_server_starts_on_this_machine_only_until_the_person_opens_the_network() {
        let (_directory, exposure) = fixture(wired());
        assert_eq!(exposure.binding().get(), Bound { host: "127.0.0.1".into(), tailscale_serve_port: None });
        assert_eq!(exposure.state(), State { mode: Mode::LocalOnly, endpoint_url: None, advertised_host: None, tailscale_serve_enabled: false, tailscale_serve_port: 443 });

        let change = exposure.set_mode(Mode::NetworkAccessible).unwrap();
        assert!(change.requires_relaunch);
        assert_eq!((change.state.mode, change.state.endpoint_url.as_deref(), change.state.advertised_host.as_deref()), (Mode::NetworkAccessible, Some("http://192.168.1.20:3774"), Some("192.168.1.20")));
        assert_eq!(exposure.binding().get().host, "0.0.0.0", "the next start listens on every interface");

        // The same choice again changes nothing and asks for no restart; going back does.
        assert!(!exposure.set_mode(Mode::NetworkAccessible).unwrap().requires_relaunch);
        let back = exposure.set_mode(Mode::LocalOnly).unwrap();
        assert!(back.requires_relaunch);
        assert_eq!(exposure.binding().get().host, "127.0.0.1");
    }

    #[test]
    fn what_was_chosen_is_there_the_next_time_the_app_starts() {
        let (directory, first) = fixture(wired());
        first.set_mode(Mode::NetworkAccessible).unwrap();
        first.set_tailscale_serve(true, Some(8443)).unwrap();
        let second = Exposure::configure(directory.path().join("desktop-settings.json"), 3774, wired(), None, vec![]);
        assert_eq!(second.state().mode, Mode::NetworkAccessible);
        assert_eq!(second.binding().get(), Bound { host: "0.0.0.0".into(), tailscale_serve_port: Some(8443) });
    }

    #[test]
    fn network_access_with_no_address_to_be_reached_at_is_refused_and_leaves_everything_as_it_was() {
        let (directory, exposure) = fixture(offline());
        let error = exposure.set_mode(Mode::NetworkAccessible).unwrap_err();
        assert_eq!(error.to_string(), "No reachable network address is available for desktop network access on port 3774.");
        assert_eq!((exposure.state().mode, exposure.binding().get().host.as_str()), (Mode::LocalOnly, "127.0.0.1"));
        assert!(!directory.path().join("desktop-settings.json").exists(), "nothing was saved");
        // An address of the tailnet is enough to be reached at, and so is one the person named.
        assert!(fixture(tailnet_only()).1.set_mode(Mode::NetworkAccessible).is_ok());
        let named = Exposure::configure(directory.path().join("other.json"), 3774, offline(), Some(" box.local ".into()), vec![]);
        assert_eq!(named.set_mode(Mode::NetworkAccessible).unwrap().state.advertised_host.as_deref(), Some("box.local"));
    }

    #[test]
    fn a_saved_choice_that_cannot_be_had_any_more_falls_back_to_this_machine() {
        let (directory, wired_exposure) = fixture(wired());
        wired_exposure.set_mode(Mode::NetworkAccessible).unwrap();
        let moved = Exposure::configure(directory.path().join("desktop-settings.json"), 3774, offline(), None, vec![]);
        assert_eq!((moved.state().mode, moved.binding().get().host.as_str()), (Mode::LocalOnly, "127.0.0.1"));
    }

    #[test]
    fn tailscale_serve_is_saved_with_its_port_and_handed_to_the_next_start() {
        let (_directory, exposure) = fixture(wired());
        let on = exposure.set_tailscale_serve(true, Some(8443)).unwrap();
        assert!(on.requires_relaunch);
        assert_eq!((on.state.tailscale_serve_enabled, on.state.tailscale_serve_port), (true, 8443));
        assert_eq!(exposure.binding().get().tailscale_serve_port, Some(8443));
        assert!(!exposure.set_tailscale_serve(true, None).unwrap().requires_relaunch, "no port keeps the saved one");
        let off = exposure.set_tailscale_serve(false, Some(8443)).unwrap();
        assert!(off.requires_relaunch);
        assert_eq!((exposure.binding().get().tailscale_serve_port, off.state.tailscale_serve_port), (None, 8443), "the port is remembered for the next time");
        assert_eq!(exposure.set_tailscale_serve(true, Some(0)).unwrap().state.tailscale_serve_port, 443, "a port out of range is the default one");
    }

    #[tokio::test]
    async fn tailscale_is_not_asked_while_the_network_and_serve_are_both_closed() {
        let (_directory, exposure) = fixture(tailnet_only());
        let program = tailscale(Some("box.tail1234.ts.net"), true);
        let endpoints = exposure.advertised_endpoints(&program).await;
        let ids: Vec<_> = endpoints.iter().map(|endpoint| endpoint.id.as_str()).collect();
        assert_eq!(ids, ["desktop-loopback:3774", "manual:https://t3.example.test"]);
        assert!(program.asked.lock().unwrap().is_empty(), "the program was not run");
    }

    #[tokio::test]
    async fn an_open_network_lists_the_tailnet_addresses_and_serve_makes_the_https_one_available_once_it_answers() {
        let (_directory, exposure) = fixture(tailnet_only());
        exposure.set_mode(Mode::NetworkAccessible).unwrap();
        let program = tailscale(Some("box.tail1234.ts.net"), true);
        let endpoints = exposure.advertised_endpoints(&program).await;
        let found: Vec<_> = endpoints.iter().map(|endpoint| (endpoint.id.as_str(), endpoint.available)).collect();
        assert_eq!(found, [("desktop-loopback:3774", true), ("manual:https://t3.example.test", true), ("tailscale-ip:http://100.101.102.103:3774", true), ("tailscale-magicdns:https://box.tail1234.ts.net/", false)]);
        assert_eq!(*program.asked.lock().unwrap(), ["status"], "serve is off: the address is not probed");

        exposure.set_tailscale_serve(true, Some(8443)).unwrap();
        let with_serve = exposure.advertised_endpoints(&tailscale(Some("box.tail1234.ts.net"), true)).await;
        let https = with_serve.iter().find(|endpoint| endpoint.id.starts_with("tailscale-magicdns:")).unwrap();
        assert_eq!((https.http_base_url.as_str(), https.available, https.hosted_app_compatible, https.reachability), ("https://box.tail1234.ts.net:8443/", true, true, Reachability::PrivateNetwork));
        let silent = exposure.advertised_endpoints(&tailscale(Some("box.tail1234.ts.net"), false)).await;
        assert!(!silent.iter().find(|endpoint| endpoint.id.starts_with("tailscale-magicdns:")).unwrap().available);
        let stopped = exposure.advertised_endpoints(&tailscale(None, false)).await;
        assert!(stopped.iter().all(|endpoint| !endpoint.id.starts_with("tailscale-magicdns:")), "no name, no HTTPS address");
    }

    #[tokio::test]
    async fn serve_alone_asks_tailscale_even_with_the_network_closed() {
        let (_directory, exposure) = fixture(wired());
        exposure.set_tailscale_serve(true, None).unwrap();
        let program = tailscale(Some("box.tail1234.ts.net"), true);
        let endpoints = exposure.advertised_endpoints(&program).await;
        assert!(endpoints.iter().any(|endpoint| endpoint.id == "tailscale-magicdns:https://box.tail1234.ts.net/" && endpoint.available));
        assert!(endpoints.iter().all(|endpoint| endpoint.reachability != Reachability::Lan), "the local network stays closed");
    }

    #[test]
    fn the_addresses_the_person_set_are_a_trimmed_comma_separated_list() {
        assert_eq!(split_endpoints(" https://a.test , ,http://b.test:9 "), ["https://a.test", "http://b.test:9"]);
        assert!(split_endpoints("").is_empty());
    }
}
