//! Remote access for the Rust server: T3 Connect (the cloud link, the managed tunnel, the relay),
//! plus the Tailscale and SSH helpers. A port of `apps/server/src/cloud/**`, `relay/**`,
//! `cli/connect.ts`, `packages/tailscale` and the pure parts of `packages/ssh`; the server wires it
//! in later (this crate is not depended on by `t3-server` yet).
//!
//! # What T3 Connect is, in one paragraph
//!
//! The environment (this server) links itself to a user's T3 Connect account by exchanging a signed
//! proof with the hosted relay (`https://relay...`, a Cloudflare Worker). The relay then provisions a
//! Cloudflare tunnel; the server runs `cloudflared` for it as a child process
//! ([`endpoint_runtime`]). Remote clients ask the relay for a short-lived credential, which the relay
//! gets from this server over that tunnel (`/api/connect/mint-credential`), bound to the client's
//! DPoP key; after that, clients talk to the server through the tunnel hostname, not the relay. All
//! relay-to-server and server-to-relay messages are EdDSA JWTs ([`jwt`]).
//!
//! # Public API
//!
//! * [`runtime::RemoteServices`] built from a [`runtime::RemoteConfig`]: `start()` when routes are
//!   serving, `shutdown()` when the server stops, `status()` a `watch` stream of
//!   [`runtime::RemoteStatus`], plus one method per HTTP route and RPC below.
//! * [`connect_cli`]: what the `t3 connect ...` subcommands read, write and print.
//! * [`tailscale::Tailscale`], [`ssh`]: helpers with an injectable executable.
//! * [`awareness`], [`delivery_proof`], [`replay`], [`boot_service`]: the smaller pieces the server
//!   calls from other features.
//!
//! # Wiring it into `t3-server`
//!
//! Services the server provides (traits in [`cloud_link`] and [`secrets`]):
//!
//! * [`secrets::SecretStore`]: delegate to `ServerSecretStore` (`get`, `set`, `create` with
//!   `already_exists`, `remove`).
//! * [`cloud_link::EnvironmentIdentity`]: the environment id and its `ExecutionEnvironmentDescriptor`.
//! * [`cloud_link::PairingIssuer`]: `AuthService::create_pairing_link` with the scopes
//!   `AUTH_STANDARD_CLIENT_SCOPES`, subject `cloud-connect`, label `T3 Connect connect`, a 2 minute
//!   TTL, and the request's `proof_key_thumbprint` (the issued session is DPoP-bound to it).
//! * [`cloud_link::AwarenessCatchUp`]: wakes the agent-activity publisher
//!   (`AgentAwarenessRelay.requestCatchUp`).
//! * [`cloud_link::UpdateHandoff`]: reads `<home>/runtime/desktop-update-restart` (a marker younger
//!   than one minute, deleted when read) and `<home>/runtime/service-state.json` (`update.status ==
//!   "pending"` with no `.service-stopping` file next to it).
//! * [`cloud_link::CliCredentials`]: [`cli_token::CliTokenManager`] already implements it.
//! * [`relay_client::Cloudflared`] as the `RelayClientResolver` and
//!   [`endpoint_runtime::ProcessConnectorSpawner`] as the `ConnectorSpawner`.
//! * [`control_plane::HttpControlPlane`], [`clock::SystemClock`], [`backoff::ThreadRandom`].
//! * `relay_url`: [`config::relay_url_config`] over `T3CODE_RELAY_URL` and the embedded default;
//!   `has_cloud_public_config`: [`config::has_cloud_public_config`]; `allow_loopback_http_relay`
//!   stays false.
//!
//! HTTP routes (all JSON; handlers decode, call one method, and map the error with
//! [`cloud_link::CloudLinkError::to_http_failure`] to `status` and `{ "_tag", "message" }`):
//!
//! | route | scope | method |
//! | --- | --- | --- |
//! | `POST /api/connect/link-proof` | `relay:write` | `link_proof(request, InboundRequest)`; the response body is the JWT string; set `cache-control: no-store` and `pragma: no-cache` |
//! | `POST /api/connect/relay-config` | `relay:write` | `apply_relay_config` -> `RelayConfigResult` |
//! | `GET /api/connect/link-state` | `relay:read` | `link_state` -> `LinkState` |
//! | `POST /api/connect/unlink` | `relay:write` | `unlink` -> `RelayConfigResult` |
//! | `POST /api/connect/preferences` | `relay:write` | `update_preferences` -> `LinkState` |
//! | `POST /api/t3-connect/health` | none (signed proof) | `answer_health_request`; no-store headers |
//! | `POST /api/connect/mint-credential`, `POST /api/t3-connect/mint-credential` | none (signed proof) | `mint_credential`; no-store headers |
//!
//! Failures on `relay-config` that are `EndpointUnavailable` answer 503 with the extra field
//! `endpointRuntimeStatus` (`HttpFailure::body`). The two `mint-credential` routes are one handler;
//! the `t3-connect` one is additionally traced as relay work.
//!
//! WebSocket RPCs: `cloud.getRelayClientStatus` (scope `relay:read`) -> `relay_client_status()`;
//! `cloud.installRelayClient` (scope `relay:write`, streaming) -> `install_relay_client()`: forward
//! each `Ok(InstallProgressEvent)` as a stream item and end with `RelayClientInstallFailedError
//! { reason, message }` on `Err(InstallError)`. Their wire shapes are the `Serialize` impls in
//! [`relay_client`]. (`rust/crates/contracts/src/permissions.rs` already lists both methods.)
//!
//! Startup and shutdown: build `RemoteServices` after the HTTP listener has an address, call
//! `start()` once routes serve (it spawns startup reconciliation, the recovery loop, and the held
//! webhook waker), and call `shutdown()` in the server's finalizer (it releases a CLI-managed tunnel
//! within 10 s and stops the connector). The webhook route calls
//! [`delivery_proof::verify_relay_delivery`] with the request headers (lower-cased) and the hook id.
//! The server's hourly job calls [`replay::prune_expired_replay_markers`] on its secrets directory
//! (replaces `auth/replayMarkers.ts`). The agent-activity publisher
//! ([`awareness`]) is driven by the server's orchestration events: project a thread into an
//! `AgentActivityState`, `sanitize_state` it, ask `PublishTracker::decide`, and
//! `publish_thread_state`; on failure schedule with `PublishRetries`.
//!
//! Settings and secrets (the keys in [`secrets`]): the link lives entirely in secret-store entries
//! (`cloud-linked-user-id`, `cloud-relay-url`, `cloud-relay-issuer`, `cloud-relay-environment-
//! credential`, `cloud-mint-ed25519-public-key`, `cloud-endpoint-runtime-config`,
//! `cloud-endpoint-confirmed-origin`, `cloud-publish-agent-activity`,
//! `cloud-hold-webhooks-while-offline`, `cloud-cli-desired-link`, `cloud-cli-oauth-token`,
//! `cloud-link-ed25519-key-pair`). There is no `settings.json` key. The descriptor capability
//! `agentActivityPublishing` is [`runtime::RemoteServices::agent_activity_publishing_active`].
//! Environment variables: `T3CODE_RELAY_URL`, `T3CODE_HOSTED_APP_URL`, `T3CODE_CLERK_PUBLISHABLE_KEY`,
//! `T3CODE_CLERK_CLI_OAUTH_CLIENT_ID`, `T3CODE_CLOUDFLARED_PATH`. Tailscale Serve keeps its server
//! settings `--tailscale-serve`, `--tailscale-serve-port` / `T3CODE_TAILSCALE_SERVE(_PORT)`: at
//! activation call `Tailscale::ensure_serve(local_port, serve_port, None)` (warn on failure), and
//! `disable_serve` in the finalizer only if it succeeded.
//!
//! CLI: `t3 connect [--headless]`, `login`, `link [--publish-only]`, `publish [--disable]`,
//! `status [--json]`, `unlink`, `logout`, all taking the project-location flags. Each maps to a
//! function in [`connect_cli`] (see its module docs); the binary owns flags, the terminal, the
//! `LoginInteraction`, and the short-lived `relay:write` session used by `unlink`/`logout` to reach
//! a running server (`run_live_cloud_unlink`).
//!
//! # What the UI needs
//!
//! `GET /api/connect/link-state` for the settings page (linked, account, relay, whether a managed
//! tunnel exists, the two toggles) and `POST /api/connect/preferences` for the toggles. For a richer
//! live view subscribe to [`runtime::RemoteServices::status`]: [`runtime::LinkPhase`] is `unlinked`,
//! `pending` (the CLI asked; links at next start), `linking`, `linked` (no tunnel), `endpoint_starting`,
//! `endpoint_up`, `endpoint_down`; `endpoint` carries the connector status (`running`, `failed` with
//! `not-installed` / `spawn-failed` / `unsupported-platform`, ...), whether the tunnel is connected,
//! and the restart count. `cloud.getRelayClientStatus` / `cloud.installRelayClient` drive the
//! "install the relay client" prompt when the status is `missing`.
//!
//! # Bugs of the original kept on purpose
//!
//! Each is marked `TODO(original ...)` at the code: a timed-out relay request reads as "unexpected
//! response" ([`relay_error`]); `clear_persisted_cloud_link` leaves the hold-webhooks opt-in behind
//! ([`cli_state`]); a whitespace-only Clerk client id is accepted ([`config`]).

pub mod awareness;
pub mod backoff;
pub mod boot_service;
pub mod cli_state;
pub mod cli_token;
pub mod clock;
pub mod cloud_link;
pub mod config;
pub mod connect_cli;
pub mod control_plane;
pub mod delivery_proof;
pub mod endpoint_runtime;
pub mod env_keys;
pub mod jwt;
pub mod link_checks;
pub mod relay_client;
pub mod relay_error;
pub mod relay_url;
pub mod replay;
pub mod runtime;
pub mod secrets;
pub mod ssh;
pub mod startup;
pub mod tailscale;
pub mod types;
pub mod util;
