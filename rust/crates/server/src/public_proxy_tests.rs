//! The preview proxy's guard and its protocol, against SOCKS5 clients the tests play and upstreams they supply. The address guard is checked
//! against what the original's block list answers for a corpus of addresses (`public-proxy-addresses.jsonl`, written by
//! `tools/generate_html_render_fixtures.mjs`); everything that touches the network goes to loopback or to a connector the test provides.
use crate::public_proxy::*;
use crate::test_support::HANG_BOUND;
use async_trait::async_trait;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
    net::TcpStream,
    sync::mpsc,
};

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(HANG_BOUND, future).await.expect("the awaited event happened")
}

fn nobody_owns_anything() -> Guard {
    Guard::with_own_addresses(Vec::new)
}

/// What the guard answers for an address in any of the spellings the corpus uses.
fn local(guard: &Guard, text: &str) -> bool {
    let bare = text.split('%').next().unwrap();
    guard.is_local(bare.parse().unwrap_or_else(|_| panic!("{text} is an address")))
}

/// Addresses the original lets through that this guard refuses, with why. Nothing else may differ.
const STRICTER: &[(&str, &str)] = &[
    ("192.0.0.0", "IETF protocol assignments"),
    ("192.0.0.8", "IETF protocol assignments"),
    ("192.0.0.170", "IETF protocol assignments"),
    ("192.0.0.255", "IETF protocol assignments"),
    ("192.0.2.1", "documentation network"),
    ("192.88.99.1", "6to4 relay anycast"),
    ("198.51.100.7", "documentation network"),
    ("203.0.113.9", "documentation network"),
    ("::ffff:0:127.0.0.1", "IPv4-translated"),
    ("::ffff:0:7f00:1", "IPv4-translated"),
    ("::ffff:0:8.8.8.8", "IPv4-translated"),
    ("100::", "discard prefix"),
    ("100::1", "discard prefix"),
    ("2001:db8::1", "documentation network"),
    ("fec0::1", "site-local"),
    ("fedf::1", "site-local"),
    ("fef0::1", "site-local"),
];

#[test]
fn the_guard_refuses_everything_the_originals_block_list_refuses_and_only_the_documented_extra() {
    let guard = nobody_owns_anything();
    let mut rows = 0;
    let mut stricter = Vec::new();
    for line in include_str!("../tests/fixtures/public-proxy-addresses.jsonl").lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let address = row["address"].as_str().unwrap();
        let (original, ours) = (row["local"].as_bool().unwrap(), local(&guard, address));
        // Never weaker.
        assert!(ours || !original, "{address}: the original refuses it and this guard does not");
        if ours && !original {
            stricter.push(address.to_owned());
        }
        rows += 1;
    }
    assert!(rows >= 120, "the corpus is {rows} addresses");
    let documented: Vec<&str> = STRICTER.iter().map(|(address, _)| *address).collect();
    // Every difference is documented, and every documented difference is real.
    for address in &stricter {
        assert!(documented.contains(&address.as_str()), "{address} is refused here and not by the original, and is not documented");
    }
    for address in &documented {
        assert!(stricter.iter().any(|found| found == address), "{address} is documented as stricter but matches the original");
    }
}

#[test]
fn the_named_hazards_are_local() {
    let guard = nobody_owns_anything();
    for address in [
        // Loopback, private, link-local, cloud metadata, carrier-grade NAT, benchmarking, multicast and reserved.
        "127.0.0.1", "127.255.255.254", "10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.169.254", "169.254.0.1", "100.64.0.1",
        "100.127.255.255", "198.18.0.1", "224.0.0.1", "239.255.255.255", "240.0.0.1", "255.255.255.255", "0.0.0.0", "0.1.2.3",
        // IPv6 loopback, unspecified, unique local, link-local, multicast.
        "::1", "::", "fc00::1", "fd12:3456::1", "fe80::1", "febf::1", "ff02::1",
        // IPv4-mapped.
        "::ffff:127.0.0.1", "::ffff:7f00:1", "::ffff:10.0.0.1", "::ffff:169.254.169.254", "::ffff:a9fe:a9fe", "::ffff:192.168.0.1",
        // NAT64: well-known, local-use. 6to4, Teredo, IPv4-compatible.
        "64:ff9b::127.0.0.1", "64:ff9b::7f00:1", "64:ff9b::a9fe:a9fe", "64:ff9b::c0a8:1", "64:ff9b:1::1", "2002:7f00:1::1", "2002:a9fe:a9fe::1",
        "2001::1", "::7f00:1", "::a9fe:a9fe",
    ] {
        assert!(local(&guard, address), "{address} must be local");
    }
    for address in ["1.1.1.1", "8.8.8.8", "93.184.216.34", "2606:4700:4700::1111", "::ffff:8.8.8.8", "64:ff9b::808:808", "2002:808:808::1", "100.63.255.255", "100.128.0.0", "172.15.255.255", "172.32.0.0"] {
        assert!(!local(&guard, address), "{address} is a public address");
    }
}

#[test]
fn an_address_this_machine_holds_is_refused_in_every_spelling_that_names_it() {
    let own = || vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), IpAddr::V6("2606:4700:4700::1111".parse().unwrap())];
    let guard = Guard::with_own_addresses(own);
    for address in ["93.184.216.34", "::ffff:93.184.216.34", "::ffff:5db8:d822", "64:ff9b::5db8:d822", "2002:5db8:d822::1", "2606:4700:4700::1111"] {
        assert!(local(&guard, address), "{address} names this machine");
    }
    // Another address on the same networks is not this machine.
    assert!(!local(&guard, "93.184.216.35"));
    assert!(!local(&guard, "2606:4700:4700::1112"));
    // The addresses are read on each decision, so one that appears later is covered.
    let late = Arc::new(Mutex::new(Vec::new()));
    let guard = {
        let late = late.clone();
        Guard::with_own_addresses(move || late.lock().unwrap().clone())
    };
    assert!(!local(&guard, "8.8.4.4"));
    late.lock().unwrap().push(IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)));
    assert!(local(&guard, "8.8.4.4"));
}

#[test]
fn numeric_spellings_of_an_ipv4_address_are_read_as_inet_aton_reads_them() {
    let cases: &[(&str, Option<[u8; 4]>)] = &[
        ("2130706433", Some([127, 0, 0, 1])),
        ("0x7f000001", Some([127, 0, 0, 1])),
        ("0X7F000001", Some([127, 0, 0, 1])),
        ("017700000001", Some([127, 0, 0, 1])),
        ("127.1", Some([127, 0, 0, 1])),
        ("127.0.1", Some([127, 0, 0, 1])),
        ("0x7f.1", Some([127, 0, 0, 1])),
        ("0177.0.0.1", Some([127, 0, 0, 1])),
        ("0x7f.0x0.0x0.0x1", Some([127, 0, 0, 1])),
        ("0xa9fea9fe", Some([169, 254, 169, 254])),
        ("2852039166", Some([169, 254, 169, 254])),
        ("0", Some([0, 0, 0, 0])),
        ("4294967295", Some([255, 255, 255, 255])),
        ("16843009", Some([1, 1, 1, 1])),
        ("010.0.0.1", Some([8, 0, 0, 1])),
        ("4294967296", None),
        ("256.0.0.1", None),
        ("1.2.3.4.5", None),
        ("1.2.3.256", None),
        ("1.2.65536", None),
        ("1.16777216", None),
        ("08", None),
        ("0x", None),
        ("0xg", None),
        ("", None),
        ("1..2", None),
        ("1.2.3.", None),
        (".1", None),
        ("example.com", None),
        ("0x7f.example.com", None),
        ("-1", None),
        ("+1", None),
    ];
    for (text, expected) in cases {
        assert_eq!(parse_legacy_ipv4(text), expected.map(Ipv4Addr::from), "{text}");
    }
}

// What a SOCKS5 client says to the proxy.
fn ipv4_target(address: [u8; 4], port: u16) -> Vec<u8> {
    let mut target = vec![1];
    target.extend_from_slice(&address);
    target.extend_from_slice(&port.to_be_bytes());
    target
}
fn ipv6_target(address: &str, port: u16) -> Vec<u8> {
    let mut target = vec![4];
    target.extend_from_slice(&address.parse::<Ipv6Addr>().unwrap().octets());
    target.extend_from_slice(&port.to_be_bytes());
    target
}
fn domain_target(host: &str, port: u16) -> Vec<u8> {
    let mut target = vec![3, host.len() as u8];
    target.extend_from_slice(host.as_bytes());
    target.extend_from_slice(&port.to_be_bytes());
    target
}

/// A connector that hands the test each upstream it is asked for, and remembers the addresses it was given.
struct TestConnector {
    asked: Mutex<Vec<Vec<SocketAddr>>>,
    upstreams: mpsc::UnboundedSender<DuplexStream>,
    fail: bool,
}
impl TestConnector {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<DuplexStream>) {
        let (upstreams, received) = mpsc::unbounded_channel();
        (Arc::new(Self { asked: Mutex::default(), upstreams, fail: false }), received)
    }
    fn failing() -> Arc<Self> {
        let (upstreams, _) = mpsc::unbounded_channel();
        Arc::new(Self { asked: Mutex::default(), upstreams, fail: true })
    }
    fn asked(&self) -> Vec<Vec<SocketAddr>> {
        self.asked.lock().unwrap().clone()
    }
}
#[async_trait]
impl Connector for TestConnector {
    async fn connect(&self, addresses: &[SocketAddr]) -> std::io::Result<Box<dyn Upstream>> {
        self.asked.lock().unwrap().push(addresses.to_vec());
        if self.fail {
            return Err(std::io::Error::other("unreachable"));
        }
        let (near, far) = tokio::io::duplex(1 << 20);
        self.upstreams.send(far).unwrap();
        Ok(Box::new(near))
    }
}

/// A resolver with an answer per name, counting how often it is asked.
#[derive(Default)]
struct TestResolver {
    answers: Mutex<Vec<(String, Vec<Vec<IpAddr>>)>>,
    asked: Mutex<Vec<String>>,
}
impl TestResolver {
    /// `name` resolves to each of `answers` in turn, the last one for every later lookup.
    fn knows(self, name: &str, answers: &[&[&str]]) -> Self {
        let parsed = answers.iter().map(|answer| answer.iter().map(|address| address.parse().unwrap()).collect()).collect();
        self.answers.lock().unwrap().push((name.to_owned(), parsed));
        self
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}
#[async_trait]
impl Resolver for TestResolver {
    async fn resolve(&self, host: &str) -> Vec<IpAddr> {
        self.asked.lock().unwrap().push(host.to_owned());
        let mut answers = self.answers.lock().unwrap();
        match answers.iter_mut().find(|(name, _)| name == host) {
            Some((_, queue)) if queue.len() > 1 => queue.remove(0),
            Some((_, queue)) => queue.first().cloned().unwrap_or_default(),
            None => Vec::new(),
        }
    }
}

struct Harness {
    proxy: PublicProxy,
    resolver: Arc<TestResolver>,
    connector: Arc<TestConnector>,
}
async fn harness(resolver: TestResolver) -> (Harness, mpsc::UnboundedReceiver<DuplexStream>) {
    let (connector, upstreams) = TestConnector::new();
    let resolver = Arc::new(resolver);
    let proxy = PublicProxy::start_with(nobody_owns_anything(), resolver.clone(), connector.clone()).await.unwrap();
    (Harness { proxy, resolver, connector }, upstreams)
}

impl Harness {
    async fn client(&self) -> TcpStream {
        TcpStream::connect((Ipv4Addr::LOCALHOST, self.proxy.port())).await.unwrap()
    }
    /// Greets and asks to connect; returns the method answer, the reply code (none if the proxy closed without one) and the open stream.
    async fn connect(&self, request: &[u8]) -> (Option<u8>, TcpStream) {
        let mut client = self.client().await;
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.write_all(&[&[5, 1, 0][..], request].concat()).await.unwrap();
        let mut answer = [0u8; 2];
        within(client.read_exact(&mut answer)).await.unwrap();
        assert_eq!(answer, [5, 0]);
        let mut reply = [0u8; 10];
        let code = match within(client.read_exact(&mut reply)).await {
            Ok(_) => Some(reply[1]),
            Err(_) => None,
        };
        (code, client)
    }
}

#[tokio::test]
async fn every_local_target_is_refused_by_address_or_by_name_and_nothing_is_connected() {
    let resolver = TestResolver::default()
        .knows("localhost", &[&["127.0.0.1", "::1"]])
        .knows("internal.example", &[&["10.1.2.3"]])
        .knows("mixed.example", &[&["93.184.216.34", "192.168.1.5"]])
        .knows("mixed-v6.example", &[&["2606:4700:4700::1111", "fd00::1"]])
        .knows("metadata.example", &[&["169.254.169.254"]])
        .knows("mapped.example", &[&["::ffff:127.0.0.1"]]);
    let (harness, _upstreams) = harness(resolver).await;
    let targets = [
        ipv4_target([127, 0, 0, 1], 80),
        ipv4_target([10, 1, 2, 3], 80),
        ipv4_target([169, 254, 169, 254], 80),
        ipv4_target([198, 18, 0, 1], 80),
        ipv4_target([100, 64, 1, 1], 443),
        ipv4_target([0, 0, 0, 0], 80),
        ipv4_target([192, 168, 0, 1], 22),
        ipv4_target([172, 16, 5, 5], 80),
        ipv4_target([224, 0, 0, 251], 5353),
        domain_target("localhost", 80),
        domain_target("internal.example", 80),
        // One local address among public ones is enough.
        domain_target("mixed.example", 443),
        domain_target("mixed-v6.example", 443),
        domain_target("metadata.example", 80),
        domain_target("mapped.example", 80),
        // An address literal sent as a name is judged as the address.
        domain_target("127.0.0.1", 80),
        domain_target("::1", 80),
        domain_target("169.254.169.254", 80),
        // IPv6 forms that carry a local IPv4 address: NAT64, 6to4, Teredo, IPv4-compatible, IPv4-mapped.
        ipv6_target("::1", 80),
        ipv6_target("fe80::1", 80),
        ipv6_target("fc00::1", 80),
        ipv6_target("64:ff9b::7f00:1", 80),
        ipv6_target("64:ff9b::a00:1", 80),
        ipv6_target("64:ff9b::a9fe:a9fe", 80),
        ipv6_target("2002:7f00:1::1", 80),
        ipv6_target("2001::1", 80),
        ipv6_target("::7f00:1", 80),
        ipv6_target("::ffff:7f00:1", 80),
        ipv6_target("::ffff:a9fe:a9fe", 80),
        ipv6_target("::ffff:0:7f00:1", 80),
    ];
    for target in targets {
        let (code, _client) = harness.connect(&target).await;
        assert_eq!(code, Some(2), "{target:?}");
    }
    assert!(harness.connector.asked().is_empty(), "no connection was attempted");
    // A name that is an address literal never reaches a resolver.
    assert!(!harness.resolver.asked().iter().any(|name| name.contains(':') || name.chars().next().is_some_and(|c| c.is_ascii_digit())), "{:?}", harness.resolver.asked());
}

#[tokio::test]
async fn hosts_spelled_in_decimal_octal_or_hex_are_judged_as_the_address_they_name_without_asking_a_resolver() {
    let (harness, mut upstreams) = harness(TestResolver::default()).await;
    for host in ["2130706433", "0x7f000001", "017700000001", "127.1", "0x7f.1", "0177.0.0.1", "0xa9fea9fe", "2852039166", "167772161", "0xa000001", "3232235777", "0300.0250.0.1"] {
        let (code, _client) = harness.connect(&domain_target(host, 80)).await;
        assert_eq!(code, Some(2), "{host}");
    }
    assert!(harness.connector.asked().is_empty());
    assert!(harness.resolver.asked().is_empty(), "{:?}", harness.resolver.asked());
    // A public address in the same spellings is connected to as the address it names.
    let (code, _client) = harness.connect(&domain_target("16843009", 443)).await;
    assert_eq!(code, Some(0));
    assert_eq!(harness.connector.asked(), vec![vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 443)]]);
    assert!(within(upstreams.recv()).await.is_some());
}

#[tokio::test]
async fn a_name_is_resolved_once_and_the_connection_goes_to_the_addresses_that_were_checked() {
    // The first lookup is public; any later one would be the loopback address of a rebinding attack.
    let resolver = TestResolver::default().knows("rebind.example", &[&["93.184.216.34", "2606:4700:4700::1111"], &["127.0.0.1"], &["127.0.0.1"]]);
    let (harness, mut upstreams) = harness(resolver).await;
    let (code, _client) = harness.connect(&domain_target("rebind.example", 8443)).await;
    assert_eq!(code, Some(0));
    assert!(within(upstreams.recv()).await.is_some());
    assert_eq!(harness.resolver.asked(), ["rebind.example"], "resolved exactly once");
    assert_eq!(
        harness.connector.asked(),
        vec![vec![SocketAddr::new("93.184.216.34".parse().unwrap(), 8443), SocketAddr::new("2606:4700:4700::1111".parse().unwrap(), 8443)]],
        "connected to the checked addresses, with the requested port"
    );
    // The next connection resolves again, and now meets the loopback answer.
    let (code, _client) = harness.connect(&domain_target("rebind.example", 8443)).await;
    assert_eq!(code, Some(2));
    assert_eq!(harness.connector.asked().len(), 1);
}

#[tokio::test]
async fn a_name_that_resolves_to_nothing_is_refused() {
    let (harness, _upstreams) = harness(TestResolver::default()).await;
    for host in ["no-such-host.invalid", ""] {
        let (code, _client) = harness.connect(&domain_target(host, 80)).await;
        assert_eq!(code, Some(2), "{host:?}");
    }
    assert!(harness.connector.asked().is_empty());
}

#[tokio::test]
async fn a_redirect_to_a_local_address_meets_the_same_check_as_any_connection() {
    let resolver = TestResolver::default().knows("site.example", &[&["93.184.216.34"]]).knows("evil.example", &[&["10.9.8.7"]]);
    let (harness, mut upstreams) = harness(resolver).await;
    // The site answers with a redirect to a private address; the proxy passes it on untouched.
    let (code, mut browser) = harness.connect(&domain_target("site.example", 80)).await;
    assert_eq!(code, Some(0));
    let mut site = within(upstreams.recv()).await.unwrap();
    browser.write_all(b"GET / HTTP/1.1\r\nHost: site.example\r\n\r\n").await.unwrap();
    let mut request = vec![0u8; 38];
    within(site.read_exact(&mut request)).await.unwrap();
    assert_eq!(request, b"GET / HTTP/1.1\r\nHost: site.example\r\n\r\n");
    let redirect = b"HTTP/1.1 302 Found\r\nLocation: http://10.9.8.7/admin\r\n\r\n";
    site.write_all(redirect).await.unwrap();
    let mut received = vec![0u8; redirect.len()];
    within(browser.read_exact(&mut received)).await.unwrap();
    assert_eq!(received, redirect);
    // Following it opens a new connection, which is refused whether it names the address or a name for it.
    for target in [ipv4_target([10, 9, 8, 7], 80), domain_target("evil.example", 80), domain_target("10.9.8.7", 80)] {
        let (code, _client) = harness.connect(&target).await;
        assert_eq!(code, Some(2));
    }
    assert_eq!(harness.connector.asked().len(), 1, "only the first connection reached the network");
}

#[tokio::test]
async fn a_malformed_or_unsupported_request_is_refused_before_anything_is_resolved() {
    let (harness, _upstreams) = harness(TestResolver::default().knows("site.example", &[&["93.184.216.34"]])).await;
    let public = ipv4_target([1, 1, 1, 1], 443);
    // Port 0 is not a connection target.
    assert_eq!(harness.connect(&ipv4_target([1, 1, 1, 1], 0)).await.0, Some(7));
    // BIND and UDP ASSOCIATE.
    for command in [2u8, 3] {
        let mut client = harness.client().await;
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.write_all(&[&[5, command, 0][..], &public].concat()).await.unwrap();
        let mut answer = [0u8; 12];
        within(client.read_exact(&mut answer)).await.unwrap();
        assert_eq!(answer[3], 7, "command {command}");
    }
    // A wrong version byte, a non-zero reserved byte and an unknown address type.
    for request in [vec![4, 1, 0, 1, 1, 1, 1, 1, 0x01, 0xbb], vec![5, 1, 9, 1, 1, 1, 1, 1, 0x01, 0xbb], vec![5, 1, 0, 9, 1, 1, 1, 1, 0x01, 0xbb]] {
        let mut client = harness.client().await;
        client.write_all(&[5, 1, 0]).await.unwrap();
        client.write_all(&request).await.unwrap();
        let mut answer = [0u8; 12];
        within(client.read_exact(&mut answer)).await.unwrap();
        assert_eq!(answer[3], 7, "{request:?}");
    }
    assert!(harness.connector.asked().is_empty());
    assert!(harness.resolver.asked().is_empty());
}

#[tokio::test]
async fn a_greeting_without_no_authentication_is_answered_with_no_acceptable_method() {
    let (harness, _upstreams) = harness(TestResolver::default()).await;
    for greeting in [vec![5u8, 1, 2], vec![5, 2, 1, 2], vec![4, 1, 0]] {
        let mut client = harness.client().await;
        client.write_all(&greeting).await.unwrap();
        let mut answer = Vec::new();
        within(client.read_to_end(&mut answer)).await.unwrap();
        assert_eq!(answer, [5, 0xff], "{greeting:?}");
    }
}

#[tokio::test]
async fn the_greeting_and_request_may_arrive_in_pieces() {
    let (harness, mut upstreams) = harness(TestResolver::default()).await;
    let mut client = harness.client().await;
    client.set_nodelay(true).unwrap();
    let request = [&[5u8, 1, 0][..], &[5, 1, 0], &ipv4_target([1, 1, 1, 1], 443)].concat();
    // One byte at a time, each flushed on its own.
    for byte in &request {
        client.write_all(&[*byte]).await.unwrap();
        client.flush().await.unwrap();
    }
    let mut answer = [0u8; 12];
    within(client.read_exact(&mut answer)).await.unwrap();
    assert_eq!(&answer[..2], [5, 0]);
    assert_eq!(answer[3], 0);
    assert!(within(upstreams.recv()).await.is_some());
}

#[tokio::test]
async fn what_the_client_sends_before_the_tunnel_opens_reaches_the_target_in_order() {
    // A resolver that answers when the test says, so the client has time to send early.
    struct Gated(tokio::sync::Notify);
    #[async_trait]
    impl Resolver for Gated {
        async fn resolve(&self, _: &str) -> Vec<IpAddr> {
            self.0.notified().await;
            vec!["93.184.216.34".parse().unwrap()]
        }
    }
    let gate = Arc::new(Gated(tokio::sync::Notify::new()));
    let (connector, mut upstreams) = TestConnector::new();
    let proxy = PublicProxy::start_with(nobody_owns_anything(), gate.clone(), connector).await.unwrap();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).await.unwrap();
    // The first bytes of the tunnel ride in the request's own segment; more follow while the name resolves.
    client.write_all(&[&[5u8, 1, 0, 5, 1, 0][..], &domain_target("late.example", 443), b"HELLO "].concat()).await.unwrap();
    let mut method = [0u8; 2];
    within(client.read_exact(&mut method)).await.unwrap();
    client.write_all(b"AND ").await.unwrap();
    client.write_all(b"MORE").await.unwrap();
    // Nothing is connected before the name resolves.
    assert!(upstreams.try_recv().is_err());
    gate.0.notify_one();
    let mut reply = [0u8; 10];
    within(client.read_exact(&mut reply)).await.unwrap();
    assert_eq!(reply[1], 0);
    let mut upstream = within(upstreams.recv()).await.unwrap();
    let mut early = vec![0u8; "HELLO AND MORE".len()];
    within(upstream.read_exact(&mut early)).await.unwrap();
    assert_eq!(early, b"HELLO AND MORE");
    // And the tunnel then carries both ways.
    client.write_all(b"ping").await.unwrap();
    let mut ping = [0u8; 4];
    within(upstream.read_exact(&mut ping)).await.unwrap();
    assert_eq!(&ping, b"ping");
    upstream.write_all(b"pong").await.unwrap();
    let mut pong = [0u8; 4];
    within(client.read_exact(&mut pong)).await.unwrap();
    assert_eq!(&pong, b"pong");
}

#[tokio::test]
async fn a_client_that_sends_too_much_before_the_tunnel_opens_is_dropped() {
    struct Never;
    #[async_trait]
    impl Resolver for Never {
        async fn resolve(&self, _: &str) -> Vec<IpAddr> {
            std::future::pending().await
        }
    }
    let (connector, _upstreams) = TestConnector::new();
    let proxy = PublicProxy::start_with(nobody_owns_anything(), Arc::new(Never), connector.clone()).await.unwrap();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).await.unwrap();
    client.write_all(&[&[5u8, 1, 0, 5, 1, 0][..], &domain_target("slow.example", 443)].concat()).await.unwrap();
    let mut method = [0u8; 2];
    within(client.read_exact(&mut method)).await.unwrap();
    // Past 64 KiB the proxy hangs up, however the writes go; the write side may see the reset first.
    let flood = vec![0u8; 16 * 1024];
    let closed = within(async {
        loop {
            if client.write_all(&flood).await.is_err() {
                return;
            }
        }
    });
    closed.await;
    assert!(connector.asked().is_empty());
}

#[tokio::test]
async fn a_client_that_leaves_while_its_name_resolves_stops_the_lookup() {
    struct Pending(Mutex<Option<tokio::sync::oneshot::Sender<()>>>);
    struct Dropped(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }
    #[async_trait]
    impl Resolver for Pending {
        async fn resolve(&self, _: &str) -> Vec<IpAddr> {
            let _lookup = Dropped(self.0.lock().unwrap().take());
            std::future::pending().await
        }
    }
    let (told, lookup_dropped) = tokio::sync::oneshot::channel();
    let (connector, _upstreams) = TestConnector::new();
    let proxy = PublicProxy::start_with(nobody_owns_anything(), Arc::new(Pending(Mutex::new(Some(told)))), connector).await.unwrap();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).await.unwrap();
    client.write_all(&[&[5u8, 1, 0, 5, 1, 0][..], &domain_target("gone.example", 443)].concat()).await.unwrap();
    let mut method = [0u8; 2];
    within(client.read_exact(&mut method)).await.unwrap();
    drop(client);
    within(lookup_dropped).await.unwrap();
}

#[tokio::test]
async fn a_target_that_cannot_be_reached_closes_the_client_without_a_reply() {
    let connector = TestConnector::failing();
    let proxy = PublicProxy::start_with(nobody_owns_anything(), Arc::new(TestResolver::default()), connector.clone()).await.unwrap();
    let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).await.unwrap();
    client.write_all(&[&[5u8, 1, 0, 5, 1, 0][..], &ipv4_target([1, 1, 1, 1], 443)].concat()).await.unwrap();
    let mut everything = Vec::new();
    within(client.read_to_end(&mut everything)).await.unwrap();
    assert_eq!(everything, [5, 0], "the method answer and nothing else");
    assert_eq!(connector.asked().len(), 1);
}

#[tokio::test]
async fn dropping_the_proxy_closes_every_connection_it_carries() {
    let (harness, mut upstreams) = harness(TestResolver::default()).await;
    // One mid-handshake, one with a tunnel open.
    let mut greeting = harness.client().await;
    greeting.write_all(&[5, 1, 0]).await.unwrap();
    let (code, mut tunnel) = harness.connect(&ipv4_target([1, 1, 1, 1], 443)).await;
    assert_eq!(code, Some(0));
    let _upstream = within(upstreams.recv()).await.unwrap();
    let Harness { proxy, .. } = harness;
    drop(proxy);
    let mut rest = Vec::new();
    // The greeting's method answer, if it was sent before the end, and then the end.
    let _ = within(greeting.read_to_end(&mut rest)).await;
    let mut tunnel_rest = Vec::new();
    let _ = within(tunnel.read_to_end(&mut tunnel_rest)).await;
    assert!(tunnel_rest.is_empty());
}

#[tokio::test]
async fn the_system_resolver_and_tcp_refuse_this_machine_by_name_and_by_address() {
    // No stub: the real resolver and connector, which never get past the guard for these.
    let proxy = PublicProxy::start().await.unwrap();
    for target in [domain_target("localhost", 80), ipv4_target([127, 0, 0, 1], 80), ipv6_target("::1", 80), domain_target("2130706433", 80), domain_target("0x7f.1", 80)] {
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).await.unwrap();
        client.write_all(&[&[5u8, 1, 0, 5, 1, 0][..], &target].concat()).await.unwrap();
        let mut answer = [0u8; 12];
        within(client.read_exact(&mut answer)).await.unwrap();
        assert_eq!(answer[3], 2, "{target:?}");
    }
}

#[tokio::test]
async fn the_tcp_connector_reaches_the_first_address_that_answers() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let open = listener.local_addr().unwrap();
    // A loopback port nothing listens on refuses at once, so the next address is tried without waiting out a stagger.
    let closed = {
        let spare = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        spare.local_addr().unwrap()
    };
    let accepted = tokio::spawn(async move { listener.accept().await.map(|(_, peer)| peer) });
    let mut stream = within(TcpConnector.connect(&[closed, open])).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    assert!(within(accepted).await.unwrap().is_ok());
    assert!(within(TcpConnector.connect(&[closed])).await.is_err());
    assert!(within(TcpConnector.connect(&[])).await.is_err());
}
