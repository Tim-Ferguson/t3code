//! The SOCKS5 proxy every connection of a headless preview browser goes through (apps/server/src/htmlRender/publicProxy.ts). It is the
//! guard that keeps a page an agent wrote off this machine and its local networks: it resolves the name a browser asks for, refuses the
//! request when any address the name resolves to is local, and otherwise connects to the addresses it checked, so a name that later resolves
//! somewhere else (DNS rebinding) changes nothing. It carries bytes only, so HTTP, TLS and WebSockets pass through unchanged, and a redirect
//! to a local address meets the same check as any other connection the browser opens.
//!
//! Chrome's Local Network Access misses some requests, such as speculation-rules prefetches; nothing misses this.
//!
//! The ranges refused are the original's, plus a few it leaves out (see `EXTRA_*`): none of them is an address a public host answers on.
use async_trait::async_trait;
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};

/// This machine and its local networks, which HTML previews must never reach.
const LOCAL_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(224, 0, 0, 0), 3),
];

/// Ranges the original leaves open that no public host answers on: IETF protocol assignments (which hold DS-Lite and NAT64 discovery
/// addresses), the three documentation networks, and the deprecated 6to4 relay anycast.
const EXTRA_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
];

/// Also the IPv6 ranges that carry an IPv4 address a translator may route to without checking it: IPv4-compatible, local-use NAT64, and Teredo.
const LOCAL_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

/// Ranges the original leaves open: IPv4-translated addresses (`::ffff:0:a.b.c.d`, which a SIIT translator hands to IPv4 unchecked), the
/// deprecated site-local network, the discard prefix and the documentation network.
const EXTRA_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::new(0, 0, 0, 0, 0xffff, 0, 0, 0), 96),
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
];

fn in_v4(address: Ipv4Addr, ranges: &[(Ipv4Addr, u8)]) -> bool {
    ranges.iter().any(|(network, prefix)| {
        let mask = if *prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(*prefix)) };
        u32::from(address) & mask == u32::from(*network) & mask
    })
}

fn in_v6(address: Ipv6Addr, ranges: &[(Ipv6Addr, u8)]) -> bool {
    ranges.iter().any(|(network, prefix)| {
        let mask = if *prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(*prefix)) };
        u128::from(address) & mask == u128::from(*network) & mask
    })
}

/// The IPv4 address a NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`) address stands for, which a translator will route to, so it is checked as
/// well. Public NAT64 targets stay reachable, as on IPv6-only networks.
fn embedded_ipv4(address: Ipv6Addr) -> Option<Ipv4Addr> {
    let groups = address.segments();
    let at = if in_v6(address, &[(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96)]) {
        6
    } else if in_v6(address, &[(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16)]) {
        1
    } else {
        return None;
    };
    let (high, low) = (groups[at], groups[at + 1]);
    Some(Ipv4Addr::new((high >> 8) as u8, high as u8, (low >> 8) as u8, low as u8))
}

/// Decides which addresses are local. The addresses this machine's interfaces hold are read on each decision, as the original does, so an
/// address that appears later is covered too.
#[derive(Clone)]
pub struct Guard {
    own_addresses: Arc<dyn Fn() -> Vec<IpAddr> + Send + Sync>,
}

impl Guard {
    /// Reads this machine's interfaces.
    pub fn system() -> Self {
        Self::with_own_addresses(|| if_addrs::get_if_addrs().map(|interfaces| interfaces.into_iter().map(|interface| interface.ip()).collect()).unwrap_or_default())
    }

    pub fn with_own_addresses(own: impl Fn() -> Vec<IpAddr> + Send + Sync + 'static) -> Self {
        Self { own_addresses: Arc::new(own) }
    }

    /// Whether `address` belongs to a local network or to this machine itself, including a public address one of its interfaces holds. An
    /// IPv4-mapped IPv6 address is judged as the IPv4 address it names.
    pub fn is_local(&self, address: IpAddr) -> bool {
        match address {
            IpAddr::V4(v4) => in_v4(v4, LOCAL_V4) || in_v4(v4, EXTRA_V4) || (self.own_addresses)().contains(&IpAddr::V4(v4)),
            IpAddr::V6(v6) => {
                if let Some(mapped) = v6.to_ipv4_mapped() {
                    return self.is_local(IpAddr::V4(mapped));
                }
                if in_v6(v6, LOCAL_V6) || in_v6(v6, EXTRA_V6) || (self.own_addresses)().contains(&IpAddr::V6(v6)) {
                    return true;
                }
                embedded_ipv4(v6).is_some_and(|embedded| self.is_local(IpAddr::V4(embedded)))
            }
        }
    }
}

/// One component of an `inet_aton` address: decimal, octal with a leading `0`, or hexadecimal with `0x`.
fn legacy_number(part: &str) -> Option<u64> {
    let (digits, radix) = match part {
        "" => return None,
        _ if part.len() > 2 && (part.starts_with("0x") || part.starts_with("0X")) => (&part[2..], 16),
        _ if part.len() > 1 && part.starts_with('0') => (&part[1..], 8),
        _ => (part, 10),
    };
    if !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// An IPv4 address spelled the way `inet_aton` reads it: one to four parts in decimal, octal or hexadecimal, the last filling the bytes left
/// (`2130706433`, `0x7f.1`, `0177.0.0.1`). Resolvers read these as addresses, so a name spelled so is judged as the address it is.
pub fn parse_legacy_ipv4(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<u64> = host.split('.').map(legacy_number).collect::<Option<_>>()?;
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let (last, leading) = parts.split_last()?;
    if leading.iter().any(|part| *part > 255) || *last >= 1u64 << (8 * (5 - parts.len())) {
        return None;
    }
    let mut value = *last;
    for (index, part) in leading.iter().enumerate() {
        value |= part << (8 * (3 - index));
    }
    Some(Ipv4Addr::from(value as u32))
}

/// Turns the name a browser asked for into the addresses it may be reached on.
#[async_trait]
pub trait Resolver: Send + Sync {
    /// Every address `host` names, in the resolver's order. Empty when it names none.
    async fn resolve(&self, host: &str) -> Vec<IpAddr>;
}

/// The system resolver.
pub struct SystemResolver;

#[async_trait]
impl Resolver for SystemResolver {
    async fn resolve(&self, host: &str) -> Vec<IpAddr> {
        match tokio::net::lookup_host((host, 0)).await {
            Ok(found) => found.map(|address| address.ip()).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// Both directions of an upstream connection.
pub trait Upstream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Upstream for T {}

/// Opens the connection to a checked address.
#[async_trait]
pub trait Connector: Send + Sync {
    async fn connect(&self, addresses: &[SocketAddr]) -> std::io::Result<Box<dyn Upstream>>;
}

/// Connects over TCP, trying every checked address, IPv6 and IPv4 alike, a new one each time the last has not answered after a short while, so
/// a host is still reached on a network whose IPv6 route is broken.
pub struct TcpConnector;

const ATTEMPT_STAGGER: Duration = Duration::from_millis(250);

#[async_trait]
impl Connector for TcpConnector {
    async fn connect(&self, addresses: &[SocketAddr]) -> std::io::Result<Box<dyn Upstream>> {
        let mut attempts: JoinSet<std::io::Result<TcpStream>> = JoinSet::new();
        let mut remaining = addresses.iter().copied();
        let mut last_error = std::io::Error::new(std::io::ErrorKind::NotFound, "no address to connect to");
        loop {
            if let Some(address) = remaining.next() {
                attempts.spawn(TcpStream::connect(address));
            } else if attempts.is_empty() {
                return Err(last_error);
            }
            let stagger = tokio::time::sleep(ATTEMPT_STAGGER);
            tokio::pin!(stagger);
            loop {
                tokio::select! {
                    done = attempts.join_next(), if !attempts.is_empty() => match done {
                        Some(Ok(Ok(stream))) => return Ok(Box::new(stream)),
                        Some(Ok(Err(error))) => {
                            last_error = error;
                            // A failed attempt makes room for the next at once.
                            break;
                        }
                        Some(Err(_)) | None => break,
                    },
                    () = &mut stagger, if remaining.len() != 0 => break,
                    else => break,
                }
            }
        }
    }
}

/// SOCKS5 (RFC 1928) replies: success, refused by rule, command unsupported.
fn reply(code: u8) -> [u8; 10] {
    [5, code, 0, 1, 0, 0, 0, 0, 0, 0]
}

/// What a client may send before its tunnel opens; a TLS hello fits easily.
const MAX_EARLY_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq)]
enum Request {
    /// More bytes are needed.
    Short,
    Malformed,
    Complete { command: u8, host: String, port: u16, rest: Vec<u8> },
}

/// The CONNECT target in a SOCKS5 request.
fn read_request(data: &[u8]) -> Request {
    if data.len() < 5 {
        return Request::Short;
    }
    // Version 5, reserved byte 0.
    if data[0] != 5 || data[2] != 0 {
        return Request::Malformed;
    }
    let end = match data[3] {
        1 => 10,
        3 => 7 + usize::from(data[4]),
        4 => 22,
        _ => return Request::Malformed,
    };
    if data.len() < end {
        return Request::Short;
    }
    let host = match data[3] {
        1 => Ipv4Addr::new(data[4], data[5], data[6], data[7]).to_string(),
        3 => data[5..5 + usize::from(data[4])].iter().map(|byte| char::from(*byte)).collect(),
        _ => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&data[4..20]);
            Ipv6Addr::from(octets).to_string()
        }
    };
    Request::Complete { command: data[1], host, port: u16::from_be_bytes([data[end - 2], data[end - 1]]), rest: data[end..].to_vec() }
}

/// The addresses to connect to for `host`, or none when it names none or any address it names is local.
async fn public_addresses(guard: &Guard, resolver: &dyn Resolver, host: &str) -> Option<Vec<IpAddr>> {
    let bare = host.split('%').next().unwrap_or(host);
    let addresses = if let Ok(address) = bare.parse::<IpAddr>() {
        vec![address]
    } else if let Some(address) = parse_legacy_ipv4(host) {
        vec![IpAddr::V4(address)]
    } else if host.is_empty() {
        Vec::new()
    } else {
        resolver.resolve(host).await
    };
    if addresses.is_empty() || addresses.iter().any(|address| guard.is_local(*address)) {
        return None;
    }
    Some(addresses)
}

/// Answers and closes. The client is never read from again, so anything it sent after its request is dropped rather than left buffered.
async fn refuse(client: &mut (impl AsyncWrite + Unpin), code: u8) {
    let _ = client.write_all(&reply(code)).await;
    let _ = client.shutdown().await;
}

struct Policy {
    guard: Guard,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
}

/// One client of the proxy, from its greeting to the end of its tunnel.
async fn serve(mut client: impl AsyncRead + AsyncWrite + Unpin, policy: Arc<Policy>) {
    let mut data: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 4096];
    let mut greeted = false;
    let (host, port, mut early) = loop {
        match client.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => data.extend_from_slice(&chunk[..read]),
        }
        if !greeted {
            if data.len() < 2 || data.len() < 2 + usize::from(data[1]) {
                continue;
            }
            // Version 5, offering "no authentication".
            if data[0] != 5 || !data[2..2 + usize::from(data[1])].contains(&0) {
                let _ = client.write_all(&[5, 0xff]).await;
                let _ = client.shutdown().await;
                return;
            }
            data.drain(..2 + usize::from(data[1]));
            greeted = true;
            if client.write_all(&[5, 0]).await.is_err() {
                return;
            }
        }
        match read_request(&data) {
            Request::Short => continue,
            Request::Malformed => return refuse(&mut client, 7).await,
            Request::Complete { command, host, port, rest } => {
                if command != 1 || port == 0 {
                    return refuse(&mut client, 7).await;
                }
                break (host, port, rest);
            }
        }
    };

    // Keeps reading while the target resolves, so a client that leaves is noticed, and holds what it sends early up to a small cap.
    let resolution = public_addresses(&policy.guard, policy.resolver.as_ref(), &host);
    tokio::pin!(resolution);
    let addresses = loop {
        tokio::select! {
            found = &mut resolution => break found,
            read = client.read(&mut chunk) => match read {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    early.extend_from_slice(&chunk[..read]);
                    if early.len() > MAX_EARLY_BYTES {
                        return;
                    }
                }
            },
        }
    };
    let Some(addresses) = addresses else { return refuse(&mut client, 2).await };
    let targets: Vec<SocketAddr> = addresses.into_iter().map(|address| SocketAddr::new(address, port)).collect();
    // A target that cannot be reached closes the client without a reply, as the original does.
    let Ok(mut upstream) = policy.connector.connect(&targets).await else { return };
    if client.write_all(&reply(0)).await.is_err() || upstream.write_all(&early).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
}

/// A running proxy on loopback. It ends, with every connection it carries, when dropped.
pub struct PublicProxy {
    port: u16,
    accept: JoinHandle<()>,
}

impl PublicProxy {
    /// Starts the proxy with the system resolver and TCP, for the life of the value.
    pub async fn start() -> std::io::Result<Self> {
        Self::start_with(Guard::system(), Arc::new(SystemResolver), Arc::new(TcpConnector)).await
    }

    /// Starts it with its decisions and its network supplied. The guard still decides every address the connector is handed.
    pub async fn start_with(guard: Guard, resolver: Arc<dyn Resolver>, connector: Arc<dyn Connector>) -> std::io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let policy = Arc::new(Policy { guard, resolver, connector });
        let accept = tokio::spawn(async move {
            // The set ends every connection with the task that holds it.
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        if let Ok((client, _)) = accepted {
                            connections.spawn(serve(client, policy.clone()));
                        }
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { port, accept })
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for PublicProxy {
    fn drop(&mut self) {
        self.accept.abort();
    }
}
