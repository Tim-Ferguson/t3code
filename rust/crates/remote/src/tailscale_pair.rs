//! `t3 pair --tailscale` (`apps/server/src/cli/pair.ts`, the Tailscale half): which local address
//! Tailscale Serve should proxy to, how to tell what already answers on the tailnet URL, and the
//! decision list that publishes the server there only when that is safe.
//!
//! The pieces the server's `pair` command calls, in order:
//!
//! 1. [`resolve_tailscale_local_target`] from the discovered server's runtime state.
//! 2. [`resolve_tailscale_pairing_base`] with a [`PairTailscale`] (the `tailscale` CLI;
//!    [`crate::tailscale::Tailscale`] implements it) and an [`EnvironmentProber`]
//!    ([`HttpEnvironmentProber`]); it returns the tailnet base URL plus the notes to print, or a
//!    [`PairError`] whose message is the one to show.
//! 3. The command mints the pairing link and prints it (QR code rendering, state-file discovery and
//!    `buildPairingUrl` stay in the server).
//!
//! Probes classify three ways because they drive different decisions ([`EnvironmentProbeResult`]).

use std::time::Duration;

use async_trait::async_trait;
use t3_contracts::ExecutionEnvironmentDescriptor;

use crate::tailscale::{
    DEFAULT_TAILSCALE_SERVE_PORT, Tailscale, TailscaleError, TailscaleReadError, TailscaleStatus,
    build_tailscale_https_base_url,
};

pub const WELL_KNOWN_ENVIRONMENT_PATH: &str = "/.well-known/t3/environment";
pub const PAIR_PROBE_TIMEOUT: Duration = Duration::from_millis(2_500);
/// Tailscale provisions an HTTPS certificate on the first request to a fresh serve mapping, which
/// can take a few seconds.
pub const TAILSCALE_PROBE_ATTEMPTS: u32 = 5;
pub const TAILSCALE_PROBE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The fields of `PersistedServerRuntimeState` (`server-runtime.json`) that decide where Serve points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairServerState {
    /// The host the server is bound to, when it recorded one.
    pub host: Option<String>,
    pub port: u16,
    /// The web dev server's URL, recorded by dev servers.
    pub dev_url: Option<String>,
}

/// `{ localPort, localHost? }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleLocalTarget {
    pub local_port: u16,
    pub local_host: Option<String>,
}

/// The errors `t3 pair` reports, each with the original's message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
    /// `NoRunningServerError`
    #[error("{}", no_running_server_message(.checked_state_paths))]
    NoRunningServer { checked_state_paths: Vec<String> },
    /// `TailscaleUnavailableError`
    #[error("Could not talk to Tailscale. Is tailscaled running? Try `tailscale status`.")]
    TailscaleUnavailable { cause: String },
    /// `MagicDnsNameMissingError`
    #[error("This machine has no MagicDNS name. Run `tailscale up` and enable MagicDNS.")]
    MagicDnsNameMissing,
    /// `ServesOtherEnvironmentError`
    #[error("Tailscale Serve on HTTPS port {serve_port} already fronts a different T3 Code server. Pass --tailscale-serve-port to publish this one on another port.")]
    ServesOtherEnvironment { serve_port: u16 },
    /// `TailscaleServeFailedError`
    #[error("tailscale serve failed for HTTPS port {serve_port}. Run `tailscale serve --https={serve_port} --bg <local-url>` by hand to see why.")]
    TailscaleServeFailed { serve_port: u16, cause: String },
    /// `ServePortOccupiedError`
    #[error("HTTPS port {serve_port} on the tailnet already serves something that is not a T3 Code server. Pass --tailscale-serve-port to publish this one on another port.")]
    ServePortOccupied { serve_port: u16 },
    /// `DevServerNotProxiableError`
    #[error("Tailscale Serve can only proxy plain-HTTP local targets, and this dev server runs at {dev_url}. Pair without --tailscale instead.")]
    DevServerNotProxiable { dev_url: String },
    /// `new URL(state.devUrl)` threw: the recorded dev URL is not a URL.
    #[error("Invalid URL")]
    InvalidDevUrl { dev_url: String },
}

fn no_running_server_message(checked: &[String]) -> String {
    let mut lines = vec!["No running T3 Code server found.".to_owned()];
    lines.extend(checked.iter().map(|path| format!("  checked {path}")));
    lines.push(
        "Start one with `npx t3 serve`, or connect this machine with T3 Connect: `npx t3 connect`."
            .to_owned(),
    );
    lines.join("\n")
}

/// `isLoopbackHost` (`startupAccess.ts`): empty and `127.*` count too.
pub fn is_loopback_host(host: Option<&str>) -> bool {
    match host {
        None | Some("") => true,
        Some(host) => {
            matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") || host.starts_with("127.")
        }
    }
}

/// `isWildcardHost`
pub fn is_wildcard_host(host: Option<&str>) -> bool {
    matches!(host, Some("0.0.0.0" | "::" | "[::]"))
}

/// `formatHostForUrl`: brackets around an IPv6 literal.
pub fn format_host_for_url(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

/// `resolveTailscaleLocalTarget`: the local endpoint Tailscale Serve should proxy to. Dev servers are
/// single-origin, so the web dev server's port is the one to publish (the backend rides along behind
/// Vite's proxy). Serve targets are always plain HTTP, so an HTTPS dev URL is rejected. A server bound
/// to one specific interface does not answer on loopback, so the proxy targets that interface.
pub fn resolve_tailscale_local_target(
    state: &PairServerState,
) -> Result<TailscaleLocalTarget, PairError> {
    if let Some(dev_url) = &state.dev_url {
        let parsed = url::Url::parse(dev_url).map_err(|_| PairError::InvalidDevUrl {
            dev_url: dev_url.clone(),
        })?;
        if parsed.scheme() != "http" {
            return Err(PairError::DevServerNotProxiable {
                dev_url: dev_url.clone(),
            });
        }
        // `URL.port` is empty for the scheme's default port, which then means 80.
        let local_port = parsed.port().unwrap_or(80);
        // `URL.hostname` keeps the brackets of an IPv6 address.
        let hostname = parsed.host_str().unwrap_or("");
        return Ok(if is_loopback_host(Some(hostname)) {
            TailscaleLocalTarget {
                local_port,
                local_host: None,
            }
        } else {
            TailscaleLocalTarget {
                local_port,
                local_host: Some(hostname.to_owned()),
            }
        });
    }
    if let Some(host) = state.host.as_deref() {
        if !is_wildcard_host(Some(host)) && !is_loopback_host(Some(host)) {
            return Ok(TailscaleLocalTarget {
                local_port: state.port,
                local_host: Some(format_host_for_url(host)),
            });
        }
    }
    Ok(TailscaleLocalTarget {
        local_port: state.port,
        local_host: None,
    })
}

/// Three outcomes, because they drive different decisions: a T3 descriptor (pair with it), nothing
/// answering (safe to configure Tailscale Serve), or something answering that is not a T3 server
/// (do NOT overwrite its mapping).
#[derive(Debug, Clone, PartialEq)]
pub enum EnvironmentProbeResult {
    Descriptor(Box<ExecutionEnvironmentDescriptor>),
    Unreachable,
    NotAT3Server,
}

/// What one HTTP answer from `<base>/.well-known/t3/environment` was.
pub fn classify_environment_response(status: u16, body: &[u8]) -> EnvironmentProbeResult {
    // A bad-gateway family answer means a proxy (Tailscale Serve) answered for a backend that is
    // gone: a stale mapping, not a live occupant. Treating it as unreachable lets `t3 pair
    // --tailscale` repair its own mapping after the server's port changed.
    if matches!(status, 502..=504) {
        return EnvironmentProbeResult::Unreachable;
    }
    // Anything else that answered HTTP but not with a valid descriptor is some other service.
    if !(200..300).contains(&status) {
        return EnvironmentProbeResult::NotAT3Server;
    }
    match serde_json::from_slice::<ExecutionEnvironmentDescriptor>(body) {
        Ok(descriptor) => EnvironmentProbeResult::Descriptor(Box::new(descriptor)),
        Err(_) => EnvironmentProbeResult::NotAT3Server,
    }
}

/// `probeEnvironmentDescriptor`
#[async_trait]
pub trait EnvironmentProber: Send + Sync {
    async fn probe(&self, base_url: &str) -> EnvironmentProbeResult;
}

/// Probes over HTTP with `reqwest`. Only getting a response is bounded by the timeout, as the
/// original bounds only the request.
pub struct HttpEnvironmentProber {
    pub timeout: Duration,
}

impl Default for HttpEnvironmentProber {
    fn default() -> Self {
        Self {
            timeout: PAIR_PROBE_TIMEOUT,
        }
    }
}

#[async_trait]
impl EnvironmentProber for HttpEnvironmentProber {
    async fn probe(&self, base_url: &str) -> EnvironmentProbeResult {
        let Ok(url) = url::Url::parse(base_url).and_then(|b| b.join(WELL_KNOWN_ENVIRONMENT_PATH))
        else {
            return EnvironmentProbeResult::Unreachable;
        };
        let call = async { reqwest::Client::new().get(url).send().await };
        // Transport failure or timeout: nothing (reachable) is listening there.
        let Ok(Ok(response)) = tokio::time::timeout(self.timeout, call).await else {
            return EnvironmentProbeResult::Unreachable;
        };
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap_or_default();
        classify_environment_response(status, &body)
    }
}

/// `awaitEnvironmentDescriptor`: probes up to five times, a second apart, until a descriptor shows
/// up; otherwise the last result.
///
/// TODO(original apps/server/src/cli/pair.ts `awaitEnvironmentDescriptor`): the one-second delay
/// also follows the last attempt, so a mapping that never answers costs one second more than
/// needed. Kept.
pub async fn await_environment_descriptor(
    prober: &dyn EnvironmentProber,
    base_url: &str,
) -> EnvironmentProbeResult {
    let mut last = EnvironmentProbeResult::Unreachable;
    for _ in 0..TAILSCALE_PROBE_ATTEMPTS {
        last = prober.probe(base_url).await;
        if matches!(last, EnvironmentProbeResult::Descriptor(_)) {
            return last;
        }
        tokio::time::sleep(TAILSCALE_PROBE_RETRY_DELAY).await;
    }
    last
}

/// The `tailscale` calls the decision list makes.
#[async_trait]
pub trait PairTailscale: Send + Sync {
    async fn read_status(&self) -> Result<TailscaleStatus, String>;
    async fn ensure_serve(
        &self,
        local_port: u16,
        serve_port: u16,
        local_host: Option<&str>,
    ) -> Result<(), String>;
}

#[async_trait]
impl PairTailscale for Tailscale {
    async fn read_status(&self) -> Result<TailscaleStatus, String> {
        Tailscale::read_status(self)
            .await
            .map_err(|error: TailscaleReadError| error.to_string())
    }

    async fn ensure_serve(
        &self,
        local_port: u16,
        serve_port: u16,
        local_host: Option<&str>,
    ) -> Result<(), String> {
        Tailscale::ensure_serve(self, local_port, Some(serve_port), local_host)
            .await
            .map_err(|error: TailscaleError| error.to_string())
    }
}

/// What `t3 pair` needs to know about the server it found.
#[derive(Debug, Clone)]
pub struct PairTarget {
    pub state: PairServerState,
    pub environment_id: String,
}

/// The tailnet URL to pair through, and the notes to print under the pairing link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscalePairingBase {
    pub base_url: String,
    pub notes: Vec<String>,
}

/// `resolveTailscalePairingBase`. Only an unreachable port, or a mapping already fronting this exact
/// environment, is safe to (re)configure; any other responder, T3 or not, never has its mapping
/// silently replaced.
pub async fn resolve_tailscale_pairing_base(
    tailscale: &dyn PairTailscale,
    prober: &dyn EnvironmentProber,
    target: &PairTarget,
    serve_port: u16,
) -> Result<TailscalePairingBase, PairError> {
    let mut notes: Vec<String> = Vec::new();
    let status = tailscale
        .read_status()
        .await
        .map_err(|cause| PairError::TailscaleUnavailable { cause })?;
    let Some(magic_dns_name) = status.magic_dns_name else {
        return Err(PairError::MagicDnsNameMissing);
    };
    // `new URL` throwing on a malformed name is a defect in the original; a name tailscale itself
    // reported is never malformed.
    let base_url = build_tailscale_https_base_url(&magic_dns_name, Some(serve_port)).map_err(
        |error| PairError::TailscaleUnavailable {
            cause: error.to_string(),
        },
    )?;

    let existing = prober.probe(&base_url).await;
    match &existing {
        EnvironmentProbeResult::Descriptor(descriptor) => {
            if descriptor.environment_id.as_str() != target.environment_id {
                return Err(PairError::ServesOtherEnvironment { serve_port });
            }
            // A matching environment id proves the mapping reaches this server, but not through
            // which port: for a dev server it may front the backend (whose /.well-known also
            // answers) while /pair only renders through the web origin. Reuse as-is for regular
            // servers; fall through and repoint our own mapping at the web port for dev servers.
            if target.state.dev_url.is_none() {
                return Ok(TailscalePairingBase { base_url, notes });
            }
        }
        EnvironmentProbeResult::NotAT3Server => {
            return Err(PairError::ServePortOccupied { serve_port });
        }
        EnvironmentProbeResult::Unreachable => {}
    }

    let local_target = resolve_tailscale_local_target(&target.state)?;
    tailscale
        .ensure_serve(
            local_target.local_port,
            serve_port,
            local_target.local_host.as_deref(),
        )
        .await
        .map_err(|cause| PairError::TailscaleServeFailed { serve_port, cause })?;
    notes.push(format!(
        "Tailscale Serve now maps {base_url} to this server and persists across restarts. Remove it with `tailscale serve --https={serve_port} off`."
    ));

    match await_environment_descriptor(prober, &base_url).await {
        EnvironmentProbeResult::Descriptor(descriptor) => {
            if descriptor.environment_id.as_str() != target.environment_id {
                return Err(PairError::ServesOtherEnvironment { serve_port });
            }
        }
        _ => notes.push(
            "The HTTPS endpoint has not answered yet. First use can take a moment while Tailscale provisions certificates."
                .to_owned(),
        ),
    }
    Ok(TailscalePairingBase { base_url, notes })
}

/// The default `--tailscale-serve-port`.
pub const DEFAULT_SERVE_PORT: u16 = DEFAULT_TAILSCALE_SERVE_PORT;
