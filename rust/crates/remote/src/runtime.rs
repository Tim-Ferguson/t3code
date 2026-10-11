//! The small API the server calls: build [`RemoteServices`] once, `start` it when routes are
//! serving, route the `/api/connect/*` handlers and the two `cloud.*` RPCs to it, watch
//! [`RemoteServices::status`] for the UI, and `shutdown` it when the server stops.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::awareness::read_publish_agent_activity_enabled;
use crate::backoff::Random;
use crate::cli_state::{DesiredLinkMode, read_cli_desired_cloud_link, read_cli_desired_link_mode};
use crate::clock::Clock;
use crate::cloud_link::*;
use crate::control_plane::ControlPlane;
use crate::endpoint_runtime::*;
use crate::relay_client::{
    InstallError, InstallProgressEvent, InstallProgressStage, RelayClientResolver,
    RelayClientStatus,
};
use crate::secrets::SecretStore;
use crate::startup::*;
use crate::types::*;

/// Shutdown gives the relay this long to delete the tunnel (`Effect.timeout("10 seconds")`).
pub const RELEASE_TUNNEL_TIMEOUT: Duration = Duration::from_secs(10);

/// Everything the remote services depend on.
pub struct RemoteConfig {
    pub secrets: Arc<dyn SecretStore>,
    pub environment: Arc<dyn EnvironmentIdentity>,
    pub pairing: Arc<dyn PairingIssuer>,
    pub awareness: Arc<dyn AwarenessCatchUp>,
    pub updates: Arc<dyn UpdateHandoff>,
    pub credentials: Arc<dyn CliCredentials>,
    pub control_plane: Arc<dyn ControlPlane>,
    pub relay_client: Arc<dyn RelayClientResolver>,
    pub connector_spawner: Arc<dyn ConnectorSpawner>,
    pub clock: Arc<dyn Clock>,
    pub random: Arc<dyn Random>,
    /// The configured relay URL, normalized (`config::relay_url_config`); `None` when unconfigured.
    pub relay_url: Option<String>,
    /// `config::has_cloud_public_config`.
    pub has_cloud_public_config: bool,
    /// The port this server listens on.
    pub local_port: Arc<dyn Fn() -> Option<u16> + Send + Sync>,
    /// See [`CloudLinkDeps::allow_loopback_http_relay`]; false outside tests.
    pub allow_loopback_http_relay: bool,
}

/// Where the link is in its life, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkPhase {
    /// Not linked and nobody asked for a link.
    Unlinked,
    /// `t3 connect link` asked for a link; it is made when the server starts.
    Pending,
    /// Startup is linking this environment now.
    Linking,
    /// Linked, without a managed tunnel (publish-only, or reached out of band).
    Linked,
    /// Linked with a managed tunnel whose connector is starting.
    EndpointStarting,
    /// The managed tunnel is carrying traffic.
    EndpointUp,
    /// Linked with a managed tunnel that is not running (stopped, failed, or restarting).
    EndpointDown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub phase: LinkPhase,
    pub link: LinkState,
    /// What the CLI wants (`t3 connect link`), if anything.
    pub desired: Option<DesiredMode>,
    pub endpoint: EndpointSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredMode {
    Managed,
    PublishOnly,
}

struct Shared {
    config: RemoteConfig,
    link: CloudLink,
    endpoint: ManagedEndpointRuntime,
    linking: AtomicBool,
    status: watch::Sender<RemoteStatus>,
    tasks: std::sync::Mutex<Vec<JoinHandle<()>>>,
}

#[derive(Clone)]
pub struct RemoteServices {
    shared: Arc<Shared>,
}

impl RemoteServices {
    pub fn new(config: RemoteConfig) -> Self {
        let endpoint = ManagedEndpointRuntime::new(
            config.relay_client.clone(),
            config.connector_spawner.clone(),
        );
        let link = CloudLink::new(CloudLinkDeps {
            secrets: config.secrets.clone(),
            environment: config.environment.clone(),
            runtime: endpoint.clone(),
            pairing: config.pairing.clone(),
            credentials: config.credentials.clone(),
            control_plane: config.control_plane.clone(),
            awareness: config.awareness.clone(),
            updates: config.updates.clone(),
            clock: config.clock.clone(),
            random: config.random.clone(),
            relay_url: config.relay_url.clone(),
            local_port: config.local_port.clone(),
            allow_loopback_http_relay: config.allow_loopback_http_relay,
        });
        let initial = Self::derive_status(&config, &endpoint, &link, false);
        let (status, _) = watch::channel(initial);
        Self {
            shared: Arc::new(Shared {
                config,
                link,
                endpoint,
                linking: AtomicBool::new(false),
                status,
                tasks: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// The link service itself, for callers that need an operation the wrappers below do not cover.
    /// Prefer the wrappers: they refresh [`Self::status`].
    pub fn link(&self) -> &CloudLink {
        &self.shared.link
    }

    pub fn endpoint(&self) -> &ManagedEndpointRuntime {
        &self.shared.endpoint
    }

    fn derive_status(
        config: &RemoteConfig,
        endpoint: &ManagedEndpointRuntime,
        link: &CloudLink,
        linking: bool,
    ) -> RemoteStatus {
        let link_state = link.link_state().unwrap_or(LinkState {
            linked: false,
            cloud_user_id: None,
            relay_url: None,
            relay_issuer: None,
            managed_tunnel_active: None,
            publish_agent_activity: false,
            hold_webhooks_while_offline: None,
        });
        let desired = read_cli_desired_cloud_link(config.secrets.as_ref())
            .unwrap_or(false)
            .then(
                || match read_cli_desired_link_mode(config.secrets.as_ref()) {
                    Ok(DesiredLinkMode::PublishOnly) => DesiredMode::PublishOnly,
                    _ => DesiredMode::Managed,
                },
            );
        let snapshot = endpoint.current();
        let phase = if linking {
            LinkPhase::Linking
        } else if !link_state.linked {
            if desired.is_some() {
                LinkPhase::Pending
            } else {
                LinkPhase::Unlinked
            }
        } else if link_state.managed_tunnel_active != Some(true) {
            LinkPhase::Linked
        } else if snapshot.status.is_running() && snapshot.connected {
            LinkPhase::EndpointUp
        } else if snapshot.status.is_running() {
            LinkPhase::EndpointStarting
        } else {
            LinkPhase::EndpointDown
        };
        RemoteStatus {
            phase,
            link: link_state,
            desired,
            endpoint: snapshot,
        }
    }

    /// Recomputes [`Self::status`] from the secret store and the connector. Called by every wrapper
    /// below and whenever the connector's status changes; call it after anything else that changes
    /// link secrets (the CLI's own unlink, for instance).
    pub fn refresh_status(&self) {
        let shared = &self.shared;
        let next = Self::derive_status(
            &shared.config,
            &shared.endpoint,
            &shared.link,
            shared.linking.load(Ordering::SeqCst),
        );
        shared.status.send_if_modified(|current| {
            if *current == next {
                false
            } else {
                *current = next;
                true
            }
        });
    }

    /// The status stream: the latest value and every change after it.
    pub fn status(&self) -> watch::Receiver<RemoteStatus> {
        self.shared.status.subscribe()
    }

    /// Starts the background work once the server's routes are serving (the activation fiber of
    /// `layerCloudDesiredLinkReconcile`): bring the stored tunnel up, register it, make a
    /// CLI-desired link, answer recovery requests, wake held webhooks when the tunnel connects.
    pub fn start(&self) {
        let shared = self.shared.clone();
        let Some(port) = (shared.config.local_port)() else {
            tracing::warn!("T3 Connect not started: the server has no listening port");
            return;
        };
        let local_origin = format!("http://127.0.0.1:{port}");
        let me = self.clone();
        let mut tasks = shared.tasks.lock().expect("tasks");
        tasks.push(tokio::spawn(run_recovery_loop(
            shared.link.clone(),
            shared.endpoint.clone(),
            shared.config.random.clone(),
            local_origin.clone(),
        )));
        tasks.push(tokio::spawn(run_held_hooks_waker(
            shared.endpoint.clone(),
            shared.config.secrets.clone(),
            shared.config.control_plane.clone(),
            shared.config.environment.environment_id(),
        )));
        // Mirror connector changes into the status stream.
        let mut snapshots = shared.endpoint.subscribe();
        let mirror = me.clone();
        tasks.push(tokio::spawn(async move {
            while snapshots.changed().await.is_ok() {
                mirror.refresh_status();
            }
        }));
        let startup = me.clone();
        tasks.push(tokio::spawn(async move {
            startup.shared.linking.store(true, Ordering::SeqCst);
            startup.refresh_status();
            run_cloud_startup(
                startup.shared.link.clone(),
                startup.shared.endpoint.clone(),
                startup.shared.config.secrets.clone(),
                startup.shared.config.random.clone(),
                StartupOptions {
                    has_cloud_public_config: startup.shared.config.has_cloud_public_config,
                    local_origin,
                },
            )
            .await;
            startup.shared.linking.store(false, Ordering::SeqCst);
            startup.refresh_status();
        }));
    }

    /// Stops the background work and the connector. Releases the CLI-managed tunnel first, within
    /// ten seconds; a failed release is logged and the next start reuses the tunnel. Returns
    /// whether a tunnel was released.
    pub async fn shutdown(&self) -> bool {
        let released = match tokio::time::timeout(
            RELEASE_TUNNEL_TIMEOUT,
            self.shared.link.release_managed_tunnel_on_shutdown(),
        )
        .await
        {
            Ok(Ok(released)) => {
                if released {
                    tracing::info!("Released the managed tunnel on shutdown");
                }
                released
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "Failed to release the managed tunnel on shutdown; the next link reuses it");
                false
            }
            Err(_) => {
                tracing::warn!(
                    "Failed to release the managed tunnel on shutdown; the next link reuses it"
                );
                false
            }
        };
        for task in self.shared.tasks.lock().expect("tasks").drain(..) {
            task.abort();
        }
        self.shared.endpoint.apply_config(None).await;
        self.refresh_status();
        released
    }

    // ---- the `/api/connect/*` handlers ------------------------------------------------------

    pub fn link_proof(
        &self,
        request: &LinkProofRequest,
        http: &crate::link_checks::InboundRequest,
    ) -> Result<String, CloudLinkError> {
        self.shared.link.link_proof(request, http)
    }

    pub async fn apply_relay_config(
        &self,
        payload: &EnvironmentConfigRequest,
    ) -> Result<RelayConfigResult, CloudLinkError> {
        let result = self.shared.link.apply_relay_config(payload).await;
        self.refresh_status();
        result
    }

    pub fn link_state(&self) -> Result<LinkState, CloudLinkError> {
        self.shared.link.link_state()
    }

    pub async fn unlink(&self) -> Result<RelayConfigResult, CloudLinkError> {
        let result = self.shared.link.unlink().await;
        self.refresh_status();
        result
    }

    pub async fn update_preferences(
        &self,
        input: PreferencesRequest,
    ) -> Result<LinkState, CloudLinkError> {
        let result = self.shared.link.update_preferences(input).await;
        self.refresh_status();
        result
    }

    pub fn answer_health_request(
        &self,
        request: &HealthRequest,
    ) -> Result<HealthResponse, CloudLinkError> {
        self.shared.link.answer_health_request(request)
    }

    pub async fn mint_credential(
        &self,
        request: &MintRequest,
    ) -> Result<MintResponse, CloudLinkError> {
        self.shared.link.mint_credential(request).await
    }

    // ---- the `cloud.*` RPCs -----------------------------------------------------------------

    /// `cloud.getRelayClientStatus`
    pub async fn relay_client_status(&self) -> RelayClientStatus {
        self.shared.config.relay_client.resolve().await
    }

    /// `cloud.installRelayClient`: streams `progress` events, then `complete` (or fails with
    /// `RelayClientInstallFailedError`). The receiver closes when the install ends.
    pub fn install_relay_client(
        &self,
    ) -> mpsc::UnboundedReceiver<Result<InstallProgressEvent, InstallError>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = self.shared.config.relay_client.clone();
        tokio::spawn(async move {
            let progress_tx = tx.clone();
            let report = move |stage: InstallProgressStage| {
                let _ = progress_tx.send(Ok(InstallProgressEvent::Progress { stage }));
            };
            match client.install_with_progress(&report).await {
                Ok(status) => {
                    let _ = tx.send(Ok(InstallProgressEvent::Complete { status }));
                }
                Err(error) => {
                    let _ = tx.send(Err(error));
                }
            }
        });
        rx
    }

    /// Whether agent-activity publishes currently leave this environment (the descriptor's
    /// capability).
    pub fn agent_activity_publishing_active(&self) -> bool {
        crate::secrets::read_agent_activity_publishing_active(self.shared.config.secrets.as_ref())
    }

    pub fn publish_agent_activity_enabled(&self) -> bool {
        read_publish_agent_activity_enabled(self.shared.config.secrets.as_ref())
    }
}
