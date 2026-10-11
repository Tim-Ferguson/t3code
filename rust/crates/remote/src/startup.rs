//! What the server does with T3 Connect while it starts and runs (`cloud/managedTunnelStartup.ts`
//! and the `layerCloudDesiredLinkReconcile` block of `server.ts`): bring the stored tunnel up,
//! register it with the relay, turn a CLI-desired link into a real one, answer recovery requests,
//! and keep the retry rules that make that safe against a relay outage or a relay fleet-wide storm.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::backoff::{Random, capped_exponential_delay, jittered};
use crate::cli_state::{DesiredLinkMode, read_cli_desired_cloud_link, read_cli_desired_link_mode};
use crate::cloud_link::{CloudLink, CloudLinkError, ManagedTunnelRegistration};
use crate::endpoint_runtime::ManagedEndpointRuntime;
use crate::secrets::{SecretStore, read_hold_webhooks_while_offline, read_relay_connection};
use crate::types::RuntimeConfig;

/// After this window the host starts its stored connector config while registration keeps
/// retrying to reconcile the origin when the relay returns.
pub const MANAGED_TUNNEL_REGISTRATION_RETRY_WINDOW: Duration = Duration::from_secs(10 * 60);
/// A host asks the relay for a replacement tunnel at most this often. Every managed host shares
/// one relay, so a host stuck in a bad loop must not become a fleet-wide request storm.
pub const MANAGED_TUNNEL_RECOVERY_COOLDOWN: Duration = Duration::from_secs(2 * 60);
/// Existing hosts register on their first boot after an upgrade, and desktop auto-update delivers
/// that boot to many hosts at once. The first registration waits a random time up to this.
pub const MANAGED_TUNNEL_FIRST_REGISTRATION_JITTER: Duration = Duration::from_secs(30);
/// The reconcile of a CLI-desired link retries this long on temporary failures.
pub const DESIRED_LINK_RECONCILE_RETRY_WINDOW: Duration = Duration::from_secs(10 * 60);
/// cloudflared registers several connections per (re)connect within a few seconds; they are one wake.
pub const HELD_HOOKS_WAKE_DEBOUNCE: Duration = Duration::from_secs(3);
const HELD_HOOKS_WAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How a registration attempt ended, as startup sees it.
#[derive(Debug, Clone, PartialEq)]
pub enum StartupRegistration {
    NotLinked,
    Ready,
    /// Registration failed for good (or exhausted its retries).
    Unavailable,
    Superseded,
    RecoveryRequired {
        config: RuntimeConfig,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum StartupAction {
    None,
    ReconcileLink,
    RequestRecovery { config: RuntimeConfig },
}

/// `managedTunnelStartupAction`.
pub fn managed_tunnel_startup_action(
    wants_cli_link: bool,
    registration: &StartupRegistration,
) -> StartupAction {
    match registration {
        StartupRegistration::RecoveryRequired { config } => StartupAction::RequestRecovery {
            config: config.clone(),
        },
        StartupRegistration::NotLinked if wants_cli_link => StartupAction::ReconcileLink,
        _ => StartupAction::None,
    }
}

impl From<ManagedTunnelRegistration> for StartupRegistration {
    fn from(registration: ManagedTunnelRegistration) -> Self {
        match registration {
            ManagedTunnelRegistration::NotLinked => Self::NotLinked,
            ManagedTunnelRegistration::Superseded => Self::Superseded,
            ManagedTunnelRegistration::RecoveryRequired { config } => {
                Self::RecoveryRequired { config }
            }
            ManagedTunnelRegistration::Ready { .. } => Self::Ready,
        }
    }
}

/// `retryManagedTunnelRegistration`: retries `op` with a jittered 1 s to 30 s exponential backoff
/// while `is_retryable` holds, for at most ten minutes. When that window runs out on a retryable
/// failure, `on_window_exhausted` runs once (startup uses it to start the stored connector) and
/// retrying continues without a bound.
pub async fn retry_managed_tunnel_registration<T, E>(
    mut op: impl FnMut() -> BoxFut<'static, Result<T, E>>,
    is_retryable: impl Fn(&E) -> bool,
    on_window_exhausted: Option<BoxFut<'static, ()>>,
    random: &dyn Random,
) -> Result<T, E> {
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if !is_retryable(&error) => return Err(error),
            Err(error) => {
                if started.elapsed() > MANAGED_TUNNEL_REGISTRATION_RETRY_WINDOW {
                    // Window over: fall back, then keep retrying without a bound.
                    drop(error);
                    if let Some(fallback) = on_window_exhausted {
                        fallback.await;
                    }
                    return retry_unbounded(op, is_retryable, random, attempt).await;
                }
                tokio::time::sleep(jittered(capped_exponential_delay(attempt), random)).await;
                attempt += 1;
            }
        }
    }
}

async fn retry_unbounded<T, E>(
    mut op: impl FnMut() -> BoxFut<'static, Result<T, E>>,
    is_retryable: impl Fn(&E) -> bool,
    random: &dyn Random,
    mut attempt: u32,
) -> Result<T, E> {
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if !is_retryable(&error) => return Err(error),
            Err(_) => {
                tokio::time::sleep(jittered(capped_exponential_delay(attempt), random)).await;
                attempt += 1;
            }
        }
    }
}

/// Failures worth retrying while registering: the relay may recover; a refusal or an unstartable
/// connector will not (`shouldRetryCloudLink && not EndpointUnavailable`).
pub fn is_retryable_registration_error(error: &CloudLinkError) -> bool {
    error.should_retry() && !error.is_endpoint_unavailable()
}

/// What startup needs to know about this process.
pub struct StartupOptions {
    /// `hasCloudPublicConfig`: without it a CLI-desired link is not even read.
    pub has_cloud_public_config: bool,
    /// `http://127.0.0.1:<port>` of this server.
    pub local_origin: String,
}

/// The stored-tunnel and desired-link work that runs once routes are serving
/// (the body of `layerCloudDesiredLinkReconcile`'s activation fiber, minus the recovery loop).
pub async fn run_cloud_startup(
    link: CloudLink,
    runtime: ManagedEndpointRuntime,
    secrets: Arc<dyn SecretStore>,
    random: Arc<dyn Random>,
    options: StartupOptions,
) {
    let local_origin = options.local_origin.clone();
    let wants_cli_link = options.has_cloud_public_config
        && read_cli_desired_cloud_link(secrets.as_ref()).unwrap_or_else(|error| {
            tracing::warn!(%error, "Failed to read the desired T3 Connect link");
            false
        });
    // A failed read must not end startup before it registers recovery. "managed" is what a
    // missing value means, so it is the safe fallback.
    let desired_mode = wants_cli_link.then(|| {
        read_cli_desired_link_mode(secrets.as_ref()).unwrap_or_else(|error| {
            tracing::warn!(%error, "Failed to read the desired T3 Connect link mode");
            DesiredLinkMode::Managed
        })
    });
    // A publish-only link must not expose the host, even if a managed config from an earlier link
    // is still stored.
    let started_confirmed = if desired_mode == Some(DesiredLinkMode::PublishOnly) {
        false
    } else {
        link.start_managed_tunnel_if_origin_confirmed(&local_origin, true)
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "Failed to start the confirmed T3 Connect tunnel");
                false
            })
    };
    let start_stored = {
        let link = link.clone();
        let origin = local_origin.clone();
        move || -> BoxFut<'static, ()> {
            let link = link.clone();
            let origin = origin.clone();
            Box::pin(async move {
                match link
                    .start_managed_tunnel_if_origin_confirmed(&origin, false)
                    .await
                {
                    Ok(true) => tracing::warn!(
                        "T3 Connect started the stored tunnel without relay confirmation"
                    ),
                    Ok(false) => {}
                    Err(error) => {
                        tracing::warn!(%error, "Failed to start the stored T3 Connect tunnel")
                    }
                }
            })
        }
    };
    let register = {
        let link = link.clone();
        let origin = local_origin.clone();
        let random = random.clone();
        let start_stored = start_stored.clone();
        move || -> BoxFut<'static, StartupRegistration> {
            let link = link.clone();
            let origin = origin.clone();
            let random = random.clone();
            let start_stored = start_stored.clone();
            Box::pin(async move {
                let op = {
                    let link = link.clone();
                    let origin = origin.clone();
                    move || -> BoxFut<'static, Result<ManagedTunnelRegistration, CloudLinkError>> {
                        let link = link.clone();
                        let origin = origin.clone();
                        Box::pin(async move {
                            link.register_managed_tunnel_recovery(&origin, true).await
                        })
                    }
                };
                let fallback = (!started_confirmed).then(|| start_stored());
                match retry_managed_tunnel_registration(
                    op,
                    is_retryable_registration_error,
                    fallback,
                    random.as_ref(),
                )
                .await
                {
                    Ok(registration) => {
                        if matches!(registration, ManagedTunnelRegistration::Ready { .. }) {
                            tracing::info!("T3 Connect managed tunnel recovery registered");
                        }
                        registration.into()
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Failed to register T3 Connect managed tunnel recovery");
                        StartupRegistration::Unavailable
                    }
                }
            })
        }
    };
    // A host without a confirmed marker is on its first boot after the upgrade. Spread those
    // registrations so an auto-update wave does not hit the relay all at once.
    if !started_confirmed && desired_mode != Some(DesiredLinkMode::PublishOnly) {
        let jitter = MANAGED_TUNNEL_FIRST_REGISTRATION_JITTER.mul_f64(random.next_f64());
        tokio::time::sleep(jitter).await;
    }
    let registration = if desired_mode == Some(DesiredLinkMode::PublishOnly) {
        StartupRegistration::NotLinked
    } else {
        register().await
    };
    // A terminal registration failure also allows the stored config to start. Transient outages
    // use the fallback inside the retry and keep registration retrying.
    if registration == StartupRegistration::Unavailable && !started_confirmed {
        start_stored().await;
    }
    match managed_tunnel_startup_action(wants_cli_link, &registration) {
        StartupAction::None => {}
        StartupAction::RequestRecovery { config } => runtime.request_recovery(config),
        StartupAction::ReconcileLink => {
            let reconciled = reconcile_desired_link_with_retry(&link, &local_origin).await;
            if reconciled == Some(DesiredLinkMode::Managed) {
                if let StartupRegistration::RecoveryRequired { config } = register().await {
                    runtime.request_recovery(config);
                }
            }
        }
    }
}

/// Reconciles a CLI-desired link, retrying temporary failures with an unjittered 1 s to 30 s
/// backoff for ten minutes. `None` when the CLI no longer wants a link or reconciling failed.
async fn reconcile_desired_link_with_retry(
    link: &CloudLink,
    local_origin: &str,
) -> Option<DesiredLinkMode> {
    let started = Instant::now();
    let mut attempt = 0u32;
    loop {
        match link
            .reconcile_desired_link_if_still_desired(local_origin)
            .await
        {
            Ok(mode) => {
                if mode.is_some() {
                    tracing::info!("T3 Connect desired link reconciled on startup");
                }
                return mode;
            }
            Err(error)
                if error.should_retry()
                    && started.elapsed() <= DESIRED_LINK_RECONCILE_RETRY_WINDOW =>
            {
                tokio::time::sleep(capped_exponential_delay(attempt)).await;
                attempt += 1;
            }
            Err(error) => {
                tracing::warn!(%error, "Failed to reconcile T3 Connect desired link on startup");
                return None;
            }
        }
    }
}

/// Answers the connector's recovery requests until cancelled. One at a time, at most one per
/// cooldown, retrying temporary failures with a jittered backoff.
pub async fn run_recovery_loop(
    link: CloudLink,
    runtime: ManagedEndpointRuntime,
    random: Arc<dyn Random>,
    local_origin: String,
) {
    let mut last_recovery: Option<Instant> = None;
    loop {
        let config = runtime.next_recovery_request().await;
        if let Some(last) = last_recovery {
            let wait = MANAGED_TUNNEL_RECOVERY_COOLDOWN.saturating_sub(last.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        last_recovery = Some(Instant::now());
        let mut attempt = 0u32;
        loop {
            match link
                .recover_managed_tunnel(&local_origin, Some(&config), true)
                .await
            {
                Ok(recovered) => {
                    if recovered {
                        tracing::info!("T3 Connect managed tunnel recovered");
                    }
                    break;
                }
                Err(error) if is_retryable_registration_error(&error) => {
                    tokio::time::sleep(jittered(
                        capped_exponential_delay(attempt),
                        random.as_ref(),
                    ))
                    .await;
                    attempt += 1;
                }
                Err(error) => {
                    tracing::warn!(%error, "Failed to recover the T3 Connect managed tunnel");
                    break;
                }
            }
        }
    }
}

/// Tells T3 Connect this environment is reachable again, so the relay delivers the webhook
/// requests it held while we were offline now rather than at its next backoff step. Nothing
/// happens unless the environment opted in (`HeldHooksWaker`). Returns whether the relay is
/// delivering held requests.
pub async fn wake_held_hooks(
    secrets: &dyn SecretStore,
    control_plane: &dyn crate::control_plane::ControlPlane,
    environment_id: &str,
) -> Result<bool, crate::relay_error::RelayRequestError> {
    if !read_hold_webhooks_while_offline(secrets) {
        return Ok(false);
    }
    let Some(connection) = read_relay_connection(secrets) else {
        return Ok(false);
    };
    control_plane
        .wake_held_hooks(
            &connection.url,
            &connection.environment_credential,
            environment_id,
        )
        .await
        .map(|response| response.pending)
}

/// Wakes held webhooks each time the managed tunnel connects, once per burst of registrations
/// (debounced), retrying a failed wake three times with a 2 s exponential backoff. The relay
/// retries on its own schedule too, so a few tries are enough.
pub async fn run_held_hooks_waker(
    runtime: ManagedEndpointRuntime,
    secrets: Arc<dyn SecretStore>,
    control_plane: Arc<dyn crate::control_plane::ControlPlane>,
    environment_id: String,
) {
    loop {
        runtime.next_tunnel_connection().await;
        // Stream.debounce: emit once the connections have been quiet for the debounce period.
        loop {
            tokio::select! {
                () = tokio::time::sleep(HELD_HOOKS_WAKE_DEBOUNCE) => break,
                () = runtime.next_tunnel_connection() => {}
            }
        }
        let mut attempt = 0u32;
        loop {
            let call = wake_held_hooks(secrets.as_ref(), control_plane.as_ref(), &environment_id);
            let outcome = tokio::time::timeout(HELD_HOOKS_WAKE_TIMEOUT, call).await;
            match outcome {
                Ok(Ok(pending)) => {
                    if pending {
                        tracing::info!("T3 Connect is delivering held webhook requests");
                    }
                    break;
                }
                _ if attempt < 3 => {
                    tokio::time::sleep(Duration::from_secs(2) * 2u32.pow(attempt)).await;
                    attempt += 1;
                }
                _ => {
                    tracing::warn!("Could not ask T3 Connect to deliver held webhook requests");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RuntimeConfig {
        RuntimeConfig::cloudflare("connector-token", Some("tunnel-1"))
    }

    #[test]
    fn recovery_is_requested_only_when_the_relay_proves_it_is_needed() {
        assert_eq!(
            managed_tunnel_startup_action(
                true,
                &StartupRegistration::RecoveryRequired { config: config() }
            ),
            StartupAction::RequestRecovery { config: config() }
        );
    }

    #[test]
    fn a_desired_cli_link_is_created_only_when_no_local_managed_link_exists() {
        assert_eq!(
            managed_tunnel_startup_action(true, &StartupRegistration::NotLinked),
            StartupAction::ReconcileLink
        );
        assert_eq!(
            managed_tunnel_startup_action(false, &StartupRegistration::NotLinked),
            StartupAction::None
        );
    }

    #[test]
    fn nothing_is_provisioned_after_a_ready_or_unavailable_registration() {
        for registration in [StartupRegistration::Ready, StartupRegistration::Unavailable] {
            assert_eq!(
                managed_tunnel_startup_action(true, &registration),
                StartupAction::None
            );
        }
    }
}
