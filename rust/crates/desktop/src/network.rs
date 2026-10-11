//! Where other devices can reach the server this app started, as pure logic: whether it listens on this machine only or on the
//! local network, which address to advertise, and the endpoints (this machine, the local network, Tailscale) a pairing link
//! can name. Port of `backend/DesktopServerExposure.ts`, `backend/tailscaleEndpointProvider.ts` and
//! `@t3tools/shared/advertisedEndpoint`. It reads no interface and runs no program: the caller passes the machine's
//! addresses and what `tailscale status` said, so nothing here binds a socket or spawns anything.
use t3_client::access::{Endpoint, Reachability};
use t3_remote::tailscale::{build_tailscale_https_base_url, is_tailscale_ipv4_address};

pub const LOOPBACK_HOST: &str = "127.0.0.1";
/// What the server binds to for the local network.
pub const LAN_BIND_HOST: &str = "0.0.0.0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    LocalOnly,
    NetworkAccessible,
}

/// One address of one network interface of this machine (`os.networkInterfaces()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub address: String,
    pub ipv4: bool,
    pub internal: bool,
}

/// `ResolvedDesktopServerExposure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposure {
    pub mode: Mode,
    pub bind_host: String,
    pub local_http_url: String,
    /// The address a device on the network is given; none when there is no reachable address.
    pub endpoint_url: Option<String>,
    pub advertised_host: Option<String>,
}

fn is_usable_lan_address(address: &str) -> bool {
    !address.starts_with("127.") && !address.starts_with("169.254.") && !is_tailscale_ipv4_address(address)
}

/// `resolveLanAdvertisedHost`: the host the person set, else the first IPv4 address of an outward interface that is not loopback,
/// link-local or Tailscale's.
pub fn lan_advertised_host(interfaces: &[Interface], explicit: Option<&str>) -> Option<String> {
    if let Some(host) = explicit.map(str::trim).filter(|host| !host.is_empty()) {
        return Some(host.to_owned());
    }
    interfaces.iter().find(|interface| !interface.internal && interface.ipv4 && is_usable_lan_address(&interface.address)).map(|interface| interface.address.clone())
}

/// `resolveDesktopServerExposure`. Local only is bound to loopback and advertises nothing beyond it.
pub fn resolve_exposure(mode: Mode, port: u16, interfaces: &[Interface], advertised_host_override: Option<&str>) -> Exposure {
    let local_http_url = format!("http://{LOOPBACK_HOST}:{port}");
    if mode == Mode::LocalOnly {
        return Exposure { mode, bind_host: LOOPBACK_HOST.into(), local_http_url, endpoint_url: None, advertised_host: None };
    }
    let advertised_host = lan_advertised_host(interfaces, advertised_host_override);
    Exposure { mode, bind_host: LAN_BIND_HOST.into(), local_http_url, endpoint_url: advertised_host.as_ref().map(|host| format!("http://{host}:{port}")), advertised_host }
}

/// `normalizeHttpBaseUrl` for the addresses built here and a person's own: a trailing slash and nothing after the host.
fn normalize(base: &str) -> Option<String> {
    let (scheme, rest) = base.split_once("://")?;
    let scheme = match scheme {
        "http" | "ws" => "http",
        "https" | "wss" => "https",
        _ => return None,
    };
    let authority = rest.split(['/', '?', '#']).next().filter(|authority| !authority.is_empty())?;
    Some(format!("{scheme}://{authority}/"))
}

fn endpoint(provider: &str, id: String, base: &str, reachability: Reachability, available: bool, hosted_app_compatible: bool, is_default: bool, label: &str) -> Option<Endpoint> {
    Some(Endpoint { id, label: label.to_owned(), http_base_url: normalize(base)?, provider_id: provider.to_owned(), reachability, hosted_app_compatible, available, is_default })
}

/// `resolveDesktopCoreAdvertisedEndpoints`: this machine, the local network when it is open, and the HTTPS addresses the person set.
/// A malformed address of the person's is left out without dropping the others.
pub fn core_endpoints(port: u16, exposure: &Exposure, custom_https_urls: &[String]) -> Vec<Endpoint> {
    let mut endpoints: Vec<Endpoint> = endpoint("desktop-core", format!("desktop-loopback:{port}"), &exposure.local_http_url, Reachability::Loopback, true, false, false, "This machine").into_iter().collect();
    if let Some(url) = &exposure.endpoint_url {
        endpoints.extend(endpoint("desktop-core", format!("desktop-lan:{url}"), url, Reachability::Lan, true, false, true, "Local network"));
    }
    for custom in custom_https_urls {
        let https = custom.starts_with("https://");
        // The user's endpoints have not been probed: available is not claimed, and only an HTTPS one can serve the hosted app.
        let Some(mut found) = endpoint("manual", format!("manual:{custom}"), custom, Reachability::Public, true, https, false, if https { "Custom HTTPS" } else { "Custom endpoint" }) else { continue };
        found.available = true;
        endpoints.push(found);
    }
    endpoints
}

/// `resolveTailscaleIpAdvertisedEndpoints`: each outward IPv4 address inside Tailscale's range, once.
pub fn tailscale_ip_endpoints(port: u16, interfaces: &[Interface]) -> Vec<Endpoint> {
    let mut seen: Vec<&str> = Vec::new();
    let mut endpoints = Vec::new();
    for interface in interfaces.iter().filter(|interface| !interface.internal && interface.ipv4 && is_tailscale_ipv4_address(&interface.address)) {
        if seen.contains(&interface.address.as_str()) {
            continue;
        }
        seen.push(&interface.address);
        let base = format!("http://{}:{port}", interface.address);
        endpoints.extend(endpoint("tailscale", format!("tailscale-ip:{base}"), &base, Reachability::PrivateNetwork, true, false, false, "Tailscale IP"));
    }
    endpoints
}

/// `resolveTailscaleMagicDnsAdvertisedEndpoint`: the MagicDNS name as an HTTPS address, available (and usable by the hosted app) only
/// when Tailscale Serve is on and the address answered a probe.
pub fn tailscale_magic_dns_endpoint(dns_name: Option<&str>, serve_enabled: bool, serve_port: Option<u16>, reachable: bool) -> Option<Endpoint> {
    let base = build_tailscale_https_base_url(dns_name?, serve_port).ok()?;
    let reachable = serve_enabled && reachable;
    endpoint("tailscale", format!("tailscale-magicdns:{base}"), &base, Reachability::PrivateNetwork, reachable, reachable, false, "Tailscale HTTPS")
}

/// Whether the Tailscale program should be asked at all: not when the person has opened neither the network nor Serve (the ask itself
/// makes macOS prompt for permission on some Tailscale builds).
pub fn should_ask_tailscale(mode: Mode, serve_enabled: bool) -> bool {
    mode == Mode::NetworkAccessible || serve_enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface(address: &str, ipv4: bool, internal: bool) -> Interface {
        Interface { address: address.into(), ipv4, internal }
    }
    fn machine() -> Vec<Interface> {
        vec![interface("127.0.0.1", true, true), interface("fe80::1", false, false), interface("169.254.10.2", true, false), interface("100.101.102.103", true, false), interface("192.168.1.20", true, false), interface("10.0.0.5", true, false)]
    }

    #[test]
    fn local_only_listens_on_loopback_and_advertises_nothing_else() {
        let exposure = resolve_exposure(Mode::LocalOnly, 3774, &machine(), Some("example.test"));
        assert_eq!((exposure.bind_host.as_str(), exposure.endpoint_url, exposure.advertised_host), ("127.0.0.1", None, None));
        assert_eq!(exposure.local_http_url, "http://127.0.0.1:3774");
    }

    #[test]
    fn the_local_network_address_skips_loopback_link_local_ipv6_and_tailscale() {
        let exposure = resolve_exposure(Mode::NetworkAccessible, 3774, &machine(), None);
        assert_eq!((exposure.bind_host.as_str(), exposure.advertised_host.as_deref(), exposure.endpoint_url.as_deref()), ("0.0.0.0", Some("192.168.1.20"), Some("http://192.168.1.20:3774")));
        // A host the person set wins; a machine with no outward address has none to advertise.
        assert_eq!(resolve_exposure(Mode::NetworkAccessible, 1, &machine(), Some(" box.local ")).advertised_host.as_deref(), Some("box.local"));
        assert_eq!(resolve_exposure(Mode::NetworkAccessible, 1, &[interface("127.0.0.1", true, true)], Some("  ")).endpoint_url, None);
    }

    #[test]
    fn the_core_endpoints_are_this_machine_the_network_and_the_addresses_the_person_set() {
        let exposure = resolve_exposure(Mode::NetworkAccessible, 3774, &machine(), None);
        let endpoints = core_endpoints(3774, &exposure, &["https://t3.example.test".into(), "http://plain.test:9".into(), "not a url".into()]);
        let shown: Vec<_> = endpoints.iter().map(|endpoint| (endpoint.id.as_str(), endpoint.http_base_url.as_str(), endpoint.reachability, endpoint.is_default, endpoint.hosted_app_compatible)).collect();
        assert_eq!(shown, vec![
            ("desktop-loopback:3774", "http://127.0.0.1:3774/", Reachability::Loopback, false, false),
            ("desktop-lan:http://192.168.1.20:3774", "http://192.168.1.20:3774/", Reachability::Lan, true, false),
            ("manual:https://t3.example.test", "https://t3.example.test/", Reachability::Public, false, true),
            ("manual:http://plain.test:9", "http://plain.test:9/", Reachability::Public, false, false),
        ]);
        let local = core_endpoints(3774, &resolve_exposure(Mode::LocalOnly, 3774, &machine(), None), &[]);
        assert_eq!(local.len(), 1);
    }

    #[test]
    fn tailscale_addresses_are_listed_once_and_only_when_inside_its_range() {
        let mut interfaces = machine();
        interfaces.push(interface("100.101.102.103", true, false));
        interfaces.push(interface("100.63.0.1", true, false));
        interfaces.push(interface("100.128.0.1", true, false));
        let endpoints = tailscale_ip_endpoints(3774, &interfaces);
        assert_eq!(endpoints.iter().map(|endpoint| endpoint.id.as_str()).collect::<Vec<_>>(), vec!["tailscale-ip:http://100.101.102.103:3774"]);
        assert_eq!((endpoints[0].label.as_str(), endpoints[0].reachability, endpoints[0].provider_id.as_str()), ("Tailscale IP", Reachability::PrivateNetwork, "tailscale"));
    }

    #[test]
    fn the_magic_dns_address_is_available_only_when_serve_is_on_and_answers() {
        let off = tailscale_magic_dns_endpoint(Some("box.tail1234.ts.net"), false, None, true).unwrap();
        assert_eq!((off.http_base_url.as_str(), off.available, off.hosted_app_compatible), ("https://box.tail1234.ts.net/", false, false));
        let silent = tailscale_magic_dns_endpoint(Some("box.tail1234.ts.net"), true, None, false).unwrap();
        assert!(!silent.available);
        let on = tailscale_magic_dns_endpoint(Some("box.tail1234.ts.net"), true, Some(8443), true).unwrap();
        assert_eq!((on.http_base_url.as_str(), on.available, on.hosted_app_compatible), ("https://box.tail1234.ts.net:8443/", true, true));
        assert!(tailscale_magic_dns_endpoint(None, true, None, true).is_none());
    }

    #[test]
    fn tailscale_is_not_asked_unless_the_network_or_serve_is_open() {
        assert!(!should_ask_tailscale(Mode::LocalOnly, false));
        assert!(should_ask_tailscale(Mode::LocalOnly, true) && should_ask_tailscale(Mode::NetworkAccessible, false));
    }
}
