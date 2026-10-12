# T3 Code Rust port

This directory contains the work-in-progress Rust replacement. The original
application remains outside this directory as the behavior and test reference.
The current Rust implementation is a foundation, not a feature-complete replacement.

From the repository root:

```sh
cargo test --locked --workspace --manifest-path rust/Cargo.toml
cargo check --locked --manifest-path rust/Cargo.toml -p t3-ui
```

The preserved native resource monitor is an independent package under
`rust/native/resource-monitor`. Its unchanged `sysinfo` implementation requires
Rust 1.95 or newer; it is excluded from the main Rust 1.89 workspace. Build it
before running the explicit native-owner integration test:

```sh
cargo build --locked --manifest-path rust/native/resource-monitor/Cargo.toml --target-dir rust/target/resource-monitor -j 2
cargo test --locked --manifest-path rust/native/resource-monitor/Cargo.toml --target-dir rust/target/resource-monitor -j 2
cargo test --locked --manifest-path rust/Cargo.toml -p t3-server native_telemetry::tests::preserved_native_monitor_real_process_table_sampling_and_history -j 2 -- --ignored
```

The regular workspace suite uses deterministic subprocess fixtures and does not
require a prebuilt monitor. The owner can discover the artifact above, or use
`T3CODE_RESOURCE_MONITOR_PATH` to select an explicit executable. The native resource service merges process counters and Electron telemetry,
with scoped sampling, history/retry RPCs and terminal port discovery. Logical-I/O
accounting is implemented and can merge recorded attribution; production
instrumentation call sites remain pending. The current Unix desktop ingress accepts explicitly transferred
pipes or socket pairs:

```sh
cargo run --locked --manifest-path rust/Cargo.toml -p t3-server -- serve --state-dir /tmp/t3-rust-isolated --mode desktop --desktop-telemetry-fd 4 --desktop-telemetry-control-fd 5
```

Those descriptors must already be inherited from the supervising desktop; they
are adopted before runtime/database startup. Ordinary web startup omits them.
A Unix supervisor can instead supply `--bootstrap-fd FD` (or
`T3CODE_BOOTSTRAP_FD`) with the original first-line JSON envelope. Acquisition
finishes before runtime/state startup. Flags override environment values, then
the envelope; `T3CODE_HOME` and envelope `t3Home` select `<home>/userdata`, while
`--state-dir` selects its directory directly. Desktop bootstrap credentials are
reusable and support the original rotating-secret windows. The envelope also
supplies telemetry channels and an explicit resource-monitor executable.
Browser IPC requests currently fail explicitly.
Automatic browser launching remains unimplemented, so `noBrowser` has no
launcher to control. Windows inherited descriptors and the desktop producer
remain pending. Host resources are
sampled on demand with a shared five-second cache. The legacy process diagnostics,
process history and scoped SIGINT/SIGKILL methods now project the resource service;
signaling requires a fresh process identity and a permitted backend category.
MacOS host sampling and owned-child signaling are tested; Linux and Windows
host/signaling implementations still require platform execution tests. Resource values retain source arithmetic and fail typed wire
validation when the original public schema cannot represent them.

Server settings load through a serialized Rust owner, watch file and symlink
target changes, and publish live configuration events. Updates move sensitive
provider variables and tokens into the isolated secret store, with rollback on
failed persistence. Provider catalogs, new terminal environments and desktop
power intervals follow settings changes. Load-time migrations restore historical
providers while preserving explicit disables, fold available project history once,
and move inline Bitbucket/GitHub tokens into the secret store. Background policy
now publishes live client leases and desktop power changes through scoped RPCs;
disconnect removes only that connection's leases. Periodic provider, VCS, usage
and Git consumers remain pending.
Storage cleanup (`storage_cleanup.rs`, `storageCleanup.ts`) runs once at start, hourly on the injected clock, when a cleanup setting changes, and when a
thread is deleted or a provider session ends while a delete rule exists. It removes worktrees (a clean linked worktree under a managed worktrees
directory, on its thread's branch, with only `node_modules` ignored, of an idle thread that nothing else uses, with no live terminal or provider
session inside it, and that is deleted, idle for `worktreeAfterDays`, already in the default branch, or the head of a merged pull request), browser
artifacts and rotated logs, with the original's rules and re-checks under a per-path workspace lease that terminals share (`workspace_lease.rs`).
The branch and the thread's `worktreePath` stay, and the next turn recreates the checkout from the branch before the provider starts
(`recreate_missing_worktree`, in the effect loop). Cleanup rules that arrive with an import are not acted on (`cleanup_hold.rs`, a Rust-port
extension): the import notes them, and until the user changes a cleanup rule the cleanup reads every rule as off; the stored values are untouched. The original does not treat pinned, settled or archived threads differently, and neither does this port;
a nested `node_modules` keeps its worktree (git lists the parent directories of an ignored install, which the original's pattern rejects). Tests run
against real repositories under a temporary directory with a local remote and a fake pull request host (`tests/storage_cleanup_flow.rs`,
`tests/storage_cleanup_turn_flow.rs`). `responseStreamingMode` holds back streaming assistant text and reasoning for every adapter except the OpenCode ones (which do their own),
at the sink every adapter commits through (`assistant_streaming.rs`, compared with the unchanged filter by `rust/tools/generate_assistant_streaming_fixtures.mjs`).
What the filter withholds is laid over the projection a planner reads (`overlay_held`, in place and taken off again), so a chunk is appended to the
whole text and an interrupted answer keeps its text; the ACP family (`acpRegistry`, Grok, Antigravity) works that way too, and its replays run in
`turn` mode as the original's harness does. `server.getConfig` and `subscribeServerConfig` carry `directEndpoints` and `remoteOpenTargets` (`environment_endpoints.rs`; the tailnet is
reached through a trait and never run by the tests) and the environment advertises `storageCleanup`, `projectWorktreeCleanup`,
`serverResolvedCommandContext` and `pullRequestStackActions` (`pull_request_stack.rs`: GitHub's merge of a stack up to a layer, polled to its end, and the
cascade rebase in a scratch clone under the temporary directory, pushed layer by layer with a lease on the reviewed head; the heads the reader reviewed
must still be the stack's, or nothing is sent). Usage-limit sources (`usage_limit_sources.rs`, `cliproxy_api.rs`) read each enabled
`usageLimitSources` entry (a CLIProxyAPI hub and its pooled Codex and Claude accounts) on the provider health interval, when the entries change and on an
untargeted `server.refreshProviders`; `subscribeServerConfig` streams them as `usageLimitSourcesUpdated` to a client that asks, offers `/usage-limits`
on the providers they report on to one that sends `usageLimitsCommand`, and `provider.consumeResetCredit { sourceId, accountId, creditId }` redeems through
the hub. Their tests serve a scripted hub from a loopback socket, with the answers of the unchanged original for the same scripts
(`tools/generate_usage_limit_source_fixtures.mjs`); Codex's `codexRateLimitsToLimits` and `codexPlanLabel` are ported there too
(`codex_usage_limits.rs`; the provider probe reads `account/rateLimits/read`, and a session's `account/rateLimits/updated` updates the instance's windows). A thread stream sends the newest running update of each tool call within a 50 ms window
(`thread_live_coalescer.rs`), and the server writes the spans browsers export, and one `ws.rpc.<method>` span per RPC call (`rpc_spans.rs`, with the original's
labels; a subscription's lasts until it ends or is interrupted), to `<logs>/server.trace.ndjson` (`trace_sink.rs`). `t3-server serve --fixture --fixture-acp-registry FILE` reads the ACP registry catalog from a file instead of
`cdn.agentclientprotocol.com`, for tests of a client (an error without `--fixture`). Usage keeps what it parsed of each transcript in
`usage-scan-cache-v5.json` (`usage_scan_cache.rs`, the original's file and document, so a state directory both servers read has one cache): a transcript that
has not changed is not read again, one that grew is read from where the last parse stopped (guarded by a hash of the bytes before that offset),
usage of transcripts since deleted stays for 90 days, and a source keeps its volume id when its directory is recreated; Grok and Cursor are still not read.
Provider event logs (`provider_event_log.rs`, `<logs>/provider/events.<thread>.log`, the original's filters, bounds, batching, rotation and retention)
are written by the Claude adapter (every frame to and from the CLI), the Codex adapter (every app-server frame, credentials redacted), the OpenCode adapter (a
structural summary of each event) and the ACP family (`acp_native_logging.rs`: a `request` line for each session-setup call of Grok, Antigravity and registry agents; the
`protocol` lines exist but, as in the original, nothing turns them on). `T3CODE_DEV_AUTH_TOKEN` (32 or more characters)
on a web server with `--dev-url` is the reusable dev credential (`auth.rs`, `ReusableDevAuth`): it signs a browser in with a cookie named for its hash and
is itself the bearer of an administrative session; desktop and normal servers ignore it. A server a service launcher started (`NODE_CHANNEL_FD` and
`T3_SERVICE_LAUNCHER_CONTEXT`) updates itself through the launcher (`launcher_ipc.rs`: the release archive and its `SHA256SUMS` are fetched from
`T3CODE_RELEASE_BASE_URL` or GitHub, the staged runtime proves itself with `__service-preflight`, and the launcher is asked to switch). ChatGPT sign-in of managed
Codex is `codex_chatgpt_auth.rs` (PKCE through a loopback callback or one the client brings, identity token checked against the issuer's keys, tokens and registrations in
the secret store under the original's names, refresh, sign-out, profile export and import), with `provider.chatgpt.*` and `provider.codex.auth-callback.subscribe`
served. A managed Codex instance has a sign-in controller in `provider_auth_service.rs`, lists itself from the sign-in (`codex_managed.rs`), and its sessions run the installed
`codex` with the access token and the token-sharing launch configuration in a shadow home (`codex_home.rs`); importing a profile ends the sessions on the old tokens
(`AuthFlow::adopt_credentials`). Text generation on a managed instance is not ported. `device.testHost` probes a
remote host over SSH (`ssh_device_host.rs`). Still open on the server: the preview/browser host registry, Cursor and analytics.
The device service runs the local host and every SSH host settings name (`device_service.rs`, `device_host.rs`; a host that resolves to this machine is left out): each host has its
own readiness, status, devices, sessions and agent-device config, an SSH host is started the first time a client lists devices, forwards its hub and agent-device ports to
loopback over `ssh -N -L`, reconnects (1 s doubling to 30 s) when the forward drops or a helper stops answering on a 10 s check, and is stopped and its config removed when settings
stop naming it. One host's lifecycle holds the service's lock while it installs and starts, as the original's does. The hosts' forwarded ports are reachable by any local user, as the
local hub is. Tests use a fake `ssh` and a loopback hub (`tests/fixtures/fake-ssh-device.py`, `device_multi_host_tests.rs`).
OTLP export (`otlp.rs`, `otlp_config.rs`) follows the original's exporters and endpoint resolution (`T3CODE_OTLP_*`, the `OTEL_*` variables, the desktop bootstrap, then settings; `http/json` by default):
traces (the server's `ws.rpc.*` spans, and a browser's export forwarded as it came, answered 502 when the collector refuses it), logs (the server's `tracing` events, without the original's `fiberId`)
and metrics. `http/protobuf` is encoded as the original's exporters encode it (`otlp_protobuf.rs`, held byte for byte to the generator's output), for the exporters and for the browser trace route. Also recorded: effect claims and queue wait, terminal sessions and restarts, webhook deliveries, durations, held delay and runs, and secret requests and refs; not the provider session and turn counters or the git command counter and timer. `tools/generate_otlp_fixtures.mjs` makes the original's bodies.
Responses are compressed as the original's router does (`response_compression.rs`: brotli, gzip or deflate by `Accept-Encoding`, text-like types of
1024 bytes or more); a body of unknown length is sent as it is.
Writing a streamed delta costs what the delta is, not what the thread is: a thread is a small skeleton (`rust_projections`) and one row per element of
each of its lists (`rust_projection_rows`, `projection_rows.rs`), a write stores the skeleton and the rows the reducer touched, and the parsed
document of the last write is kept in memory (`projection_cache.rs`) so a write and the shell stream parse and copy nothing of it. The history a thread
shows (`visibleTurnItems`) replaces the row an event wrote instead of being rebuilt, and the shell stream reads each thread once for the events that are
waiting and sends a shell only when it changed (`skipUnchangedThreadShells`). Provider events keep no command receipt, as in the original.
`src/stream_bench.rs` prints what a delta and a cold start cost by thread length, and its guard counts the work.
Device-host settings resolve SSH aliases without connecting, exclude only self
targets, and preserve proxies, forwarded ports and unresolved entries. Local device
state and session lifecycle, consent configuration and agent-daemon bootstrap,
reuse and shutdown are implemented. Other toolkit injection pipelines remain unfinished; the guarded launcher uses an externally installed
agent-device CLI.

Existing state of the original server is imported once into this server's own store
(`rust-state.sqlite`, beside the original's `statev2.sqlite` or V1 `state.sqlite`, which are
never written or removed). `t3-server import --from <T3 home> --state-dir <dir>` runs it by
hand; `serve` runs it on first start in a state directory that holds the original's database
and no store of this server (`--no-import` skips it; the original must not be running).
Threads, projects and their history come from the event log, replayed under the sequences
the original gave them through the same reducers the server uses (every row decoded
strictly; a row that does not decode names the event and stops the import), and V1 threads
and projects that the original would itself have imported on its next start are imported
with their messages. Command receipts, scheduled tasks and their webhook deliveries,
viewed-file marks and MCP app context come across; settings, keybindings, attachments and
the settings secrets (and the ChatGPT sign-in of managed Codex) are used in place when the state directory is the original's, and are
copied otherwise, with the usage scan caches (settings load-time migrations then run on first start as ever). The import
builds `rust-state.sqlite.importing`, resumes it after an interruption when the source is
unchanged, and renames it into place only when complete and checked; `rust_import_record`
records the source's schema version and counts, so a second start or a second command
imports nothing, and a source written by a newer original than this build reads (schema
migration above 59) is refused. Work in flight is not carried: the effect outbox, pending
launches, signed-in sessions and T3 Connect's link key stay behind; runs and requests the
source left open are settled by the server's ordinary startup recovery, and devices pair
again. The environment keeps the original's identity (`environment-id` is written to
`rust-environment-id`), so saved connections, client state and the check a client makes that it
reached the server it meant still name it; an identity this server already has with a store
behind it is kept, and the import says so. Usage that the original kept for transcripts since
deleted (its scan cache) is used in place when the state directory is the original's, and not copied otherwise. What the import serves is held to what the original
served for the same state (`tests/state_import_flow.rs`): every field of the shell and of each
thread's projection, with a short list of differences that are intended and why.

Process contract of `t3-server serve`, for a supervisor. Once the socket is bound and every service is running, the server
writes one line to standard output: `T3_SERVER_READY ` followed by a JSON object
`{"pid":N,"host":"127.0.0.1","port":P,"origin":"http://127.0.0.1:P"}` (`host` is the address bound, `origin` one a client on the
same machine can use; with `--port 0` this is where the port is learned). The original has no such line (its desktop polls
`/.well-known/t3/environment`, which still works). SIGINT, SIGTERM (what the original desktop stops its backend with) and SIGHUP
end the server the same way: executions, terminals and the other services stop, open connections get five seconds to finish, the
store is closed and the runtime lock of the state directory is released. With `--mode desktop` the server also stops when the
process that started it is gone (a changed parent process id, noted at startup); the original has no such watch, and without it a
backend left behind by a desktop that was killed keeps the state directory's lock. A lock whose holder died is released by the
operating system, so none is ever stale. On Windows the bootstrap envelope is read from standard input (`--bootstrap-fd 0`, the
delivery the original's desktop uses across the WSL boundary); the original maps an inherited pipe handle to a descriptor number,
which this server does not.

Usage reads OpenCode and Antigravity as well as Claude Code and Codex (`usage_readers.rs`: OpenCode's databases and
pre-migration JSON files under `OPENCODE_DATA_DIR`, Antigravity's per-conversation databases under `ANTIGRAVITY_DATA_DIR`, the
default `~/.gemini/antigravity*` directories and each Antigravity instance's profile), opened read-only on every scan (the original reads them afresh on every scan too and caches only transcripts), with the
original's alias merging; `tests/usage_readers_flow.rs` replays what the unchanged readers made of the same stores
(`rust/tools/generate_usage_reader_fixtures.mjs`). Grok and Cursor are still not read. The pull request sync has a flow test
(`tests/pull_request_sync_flow.rs`) over a fake GitHub; a link also holds the host's stack (read when its snapshot or stack membership moved) and an active
thread is linked to the other layers as `stack` links.

T3 Connect (`t3-remote`, wired by `server/src/connect.rs`) is built once the listener has an address and started when the routes
serve; it is built whether or not anything is configured, as the original's `CloudLink` is, and without a relay URL and Clerk
keys (`T3CODE_RELAY_URL`, `T3CODE_CLERK_PUBLISHABLE_KEY`, `T3CODE_CLERK_CLI_OAUTH_CLIENT_ID`; the Rust build embeds none) it
reads only what is stored and makes no request. It serves `POST /api/connect/link-proof`, `relay-config`, `unlink`,
`preferences`, `GET /api/connect/link-state` (sessions with `relay:write`/`relay:read`), and the proof-authenticated
`POST /api/t3-connect/health` and `/api/{connect,t3-connect}/mint-credential`; the RPCs `cloud.getRelayClientStatus` and
`cloud.installRelayClient` (a stream); relay-signed delivery proofs on the webhook route; the descriptor capability
`agentActivityPublishing` (read per request); and the hourly sweep of replay markers. Credentials the relay vouches for are
DPoP-bound (`dpop.rs`): `/oauth/token` with a `DPoP` header issues a one-hour `dpop-access-token` session bound to the proof's
key, and every request with `Authorization: DPoP` proves the key again, once per proof (replay markers are secrets named
`dpop-proof-*`, as in the original). `agent_awareness.rs` publishes each thread's state to the relay (the original's
`AgentAwarenessRelay`): committed events pass `should_publish`, the thread shell is projected, and `t3_remote::awareness` decides.
`--tailscale-serve [--tailscale-serve-port N]` (or `T3CODE_TAILSCALE_SERVE[_PORT]`, or the desktop bootstrap) runs
`tailscale serve` once the server runs and switches it off at shutdown only if it was switched on. `t3-server connect
[--headless] | login | link [--publish-only] | publish [--disable] | status [--json] | unlink | logout` (`--base-dir` names the T3
home) are the original's `t3 connect` commands; the running server writes `server-runtime.json` so `unlink` can ask it to stop its
tunnel. Not ported: the offer to install the background service at the end of `t3 connect`. The relay, Clerk, GitHub releases and
a Tailscale daemon are never contacted by the tests (loopback fakes only); the real hosted service is unverified.

One-shot text generation (commit messages, change request content, branch names, thread titles) runs through Codex, Claude, Pi,
OpenCode (1.x and 2.x, by what the instance's probe says the server is) and Antigravity (`text_generation_*.rs`); Cursor and Grok are not
written for. OpenCode runs the prompt in a temporary session on the instance's own server with every tool refused (1.x denies, 2.x asks
and rejects each ask as it arrives; 2.x removes the session afterwards, as the original does and its 1.x path does not). Antigravity runs
a short ACP session of the instance's own login in an empty temporary directory, with no MCP servers and no client tools, and fails the
request on any tool call, permission or question, written file, output beyond 128,000 UTF-16 units or model the account does not offer;
a profile with global hooks or MCP configuration cannot write text (its `supportsTextGeneration` is false). Not ported: the original
registers the Antigravity helper with the sign-in flow so a sign-out stops it (`withProcess`), and tells an OpenCode 2.x execution end it
cannot decode from a readable one by its schema (here the readable reason is reported).

The MCP OAuth server (`mcp_oauth.rs`, `mcp_oauth_http.rs`; `McpOAuth.ts`) lets agents T3 Code did not launch sign in to `/mcp`:
`/.well-known/oauth-protected-resource[/mcp]`, `/.well-known/oauth-authorization-server`, `/oauth/mcp/{register,authorize,approval,decision,token}`,
issuer and resource from the request's own `Host` (https behind `x-forwarded-proto: https`). A client id is its signed metadata
(HMAC-SHA256 with the secret `mcp-oauth-signing-key`, byte-identical to the original's, so ids survive a restart and an import), a code lives in
memory for 60 seconds and is spent by the first request that presents it, PKCE is S256 only, and the token is a 30-day bearer session of the
`mcp-client` kind that only `/mcp` accepts (every other route refuses it). Approval is by a one-time pairing code (spent even when its scopes
fall short; a code bound to a DPoP key is refused unspent) or one click of a browser session holding `access:write` and the scopes of the access
chosen. A request to `/mcp` without a credential that looks like a provider token is pointed at the metadata (`resource_metadata`).

The command line besides the server (`cli_commands.rs`): `--version`/`-v` prints `t3 v<version>`; `pair [--scope S]... [--ttl D] [--label L]`
prints the credential alone (with `--tailscale [--tailscale-serve-port N]` it publishes the running server on the tailnet as the original does
and prints the pairing URL with its QR code); `auth pairing create|list|revoke` and `auth session issue|list|revoke` print what the original's
do; `project add|remove|rename` runs against the running server when one answers and on the state otherwise; `start` is `serve`, and
`--no-browser` is accepted (nothing opens a browser). The hidden `__ssh-helper pick-port|wait-ready|runtime-port` and
`__service-preflight` are what the remote SSH launch and a staged update run. The location flags are `--base-dir` (its `userdata`),
`--state-dir` and `T3CODE_HOME`, else `.t3-rust` where `serve` keeps its state. Without `--assets` the server serves the web app bundled beside
its executable (`client/index.html`) so the launch script's readiness probe of `/` gets a 2xx. `theme set|clear|show` (`cli_theme.rs`)
writes `defaultTheme` and `defaultThemeSetAt` into `settings.json` and publishes a theme file into `<state dir>/themes`, exactly as the
original does (compared with the original's own runs in `tests/environment_theme_flow.rs`); a failure prints its message alone, where the
original prints a log record with a timestamp and a source position, and a flag error prints one line where the original prints the
command's help page. `serve` takes the positional `cwd` (made when missing), `--dev-url` (or `VITE_DEV_SERVER_URL`: loopback requests for
the page are redirected to it, and the proxied API paths answer 404), `--auto-bootstrap-project-from-cwd` (a project for the directory
and a first thread, named in the `welcome` event) and `--log-websocket-events` (the router logs each request). Not done: `service
install|restart|uninstall|status`, `update` and `uninstall` (they change the user's system; they would need the boot-service files
`t3_remote::boot_service*` write, the pinned runtime and the self-update download wired to a real release feed), `app` (the desktop
activation socket), `browser setup`, `trace summary`, `triage`, the per-command help pages of the original's CLI, and `connect` is as
before.

The environment themes (`environment_theme.rs`) are the palettes a machine publishes as `<state dir>/themes/<id>.json`: the directory is
watched, read through one guarded handle per file (no symlink, no special file, 32 KiB each, 32 files, 192 KiB in all), and streamed as
`environmentThemesUpdated` to the subscribers of `subscribeServerConfig` that asked for them (`environmentThemes`). `server.updateServer`,
`server.updateServerWithProgress` and `server.commitDesktopUpdate` answer as the original does: a server the desktop app started with its
control channel drives the app's own update over that channel (`desktop_app_update.rs`), a server a service launcher started updates through
the launcher (`launcher_ipc.rs`), and every other server answers that remote updates need the background service. The web app route (`static_route.rs`) serves files with the original's validators (a weak `ETag`, `Last-Modified`, `304`),
cache policy (assets named in `.vite/manifest.json` are immutable) and status answers. The
descriptor's label and machine kind come from the machine itself (`environment_host.rs`). `rust/tools/check_rpc_surface.mjs` lists what the
original exposes (RPC methods, HTTP routes, MCP tools, settings keys, environment capabilities) that this server does not serve, from
source on both sides; its allowlist (`check_rpc_surface_allowlist.json`) carries a reason for each gap that is known.

A start that finds the original's database and no store of this server's own imports it first (see above). When that import
fails the server does not open a store: it serves the environment descriptor, the web assets (`--assets`) and the failure at
`GET /api/rust/state-import` (`{"stateImport": {...}}`), so a client can show what went wrong, and `--strict-import` makes it exit
with an error instead. After a successful import the server config (`server.getConfig`, the snapshot of `subscribeServerConfig`)
carries a `stateImport` object (status, counts of projects and threads imported, what was read and not carried, warnings, what the
user has to enter again) until `server.acknowledgeStateImport` is called. These are Rust-port extensions
(`t3_contracts::state_import`); the original has no such state.

The workspace contains shared JSON contracts (`t3-contracts`), client connection,
RPC and projection state (`t3-client`), SQLite event/receipt/outbox persistence and
project commands and HTTP/WebSocket transport (`t3-server`), and a Dioxus UI (`t3-ui`). Contracts
preserve unported nested payloads as JSON. Compatibility tests cover legacy model
selection, provider slugs, authentication grants, forward event decoding, and
Effect RPC envelopes. Server tests cover atomic commits, durable command receipts,
project validation, and effect leases.

The UI implements the project/thread sidebar, conversation and composer,
approval and user-input responses, earlier-history loading, command output,
file changes, plans and search activities, remote server connections, provider
settings, and theme selection. Provider settings expose source-derived fields,
enabled controls and per-instance removal/reset through atomic server mutations.
Provider-only edits require the destination's `providers:manage` permission; mixed
patches retain their additional settings permissions. Tests cover opaque config
and sibling preservation, grant revocation and replacement connections while
edits are pending. Account controls consume the advertised sign-in methods and
live authentication stream, with browser consent, configured-environment guidance,
ephemeral credential fields, an owned terminal, cancellation and sign out.
The page refreshes provider status, shows each provider's version advisory and runs its
update after a confirmation that names the command, installs and removes the managed
Codex runtime with a live progress row, manages an ACP agent's native sessions, providers
and sign-out, adds an instance (a local ACP command included) and edits command
arguments. Adding an instance from the ACP registry search, automatic callback
coordination and complete provider/model preferences remain unfinished.
The pull request panel has the original's tabs (Summary, Timeline, Code) and its composer: a
comment, or a review that carries the line comments collected on the Code tab. It also reacts,
rewrites the title, description and your own remarks, files a pull request under a thread (shown
in the thread header and as the badge of its sidebar row), starts a thread from one in a dialog,
and lists the environment's pull requests. Where the host keeps stacks (GitHub's), the panel reads the stack with the
pull request and shows it as `Stack N, layer p of s` with the layers under it (`pull_request_stack.rs`, `t3_client::pull_request_stack`,
`PullRequestStackMenu.tsx`): "Merge stack" merges the layers from the bottom up to the one in view, "Rebase stack" rebases every
open layer, each after a question that names the layers, and each naming the heads that were read, so the server refuses a stack
that moved ("The stack changed. Refresh it before trying again."). The stack is read again after every answer, because the layers
of a rebase that stopped stay pushed; a pull request in a stack is not merged on its own, and until the lookup has settled it is not
merged at all (`allowsSinglePullRequestMerge`). The actions need `pullRequestStackActions` on the environment, `stackActions` on
the host, `stackRebase` on the account for a rebase, and a stack read just now. Not ported: the stack that the thread's saved
links remember (`savedPullRequestStack`), which the original shows while a refresh fails.
The tabbed panel beside the conversation (`right_panel.rs`, `t3_client::right_panel`, `RightPanelTabs.tsx`/`rightPanelStore.ts`) holds
the Diff, Files, file, pull request and linked pull requests tabs of each thread (and of the new-thread form, which opens
files), with the tab bar, the "+" menu and the launcher an empty panel shows (letters F, D, P, L), the menu of a tab, hide and
maximize. The tabs are kept per thread in the browser's storage (`t3code:rust-right-panel:v1`) and a thread's delete takes its
panel with it; closed tabs are remembered (`t3code:rust-closed-views:v1`, twenty) for `view.reopenClosed`, which opens the tab in its own
thread. `rightPanel.toggle`, `rightPanel.new`, `rightPanel.toggleMaximized`, `rightPanel.close`, `view.reopenClosed` and, on the web build,
`navigation.back`/`forward` are on the Keybindings page. The terminal stays a drawer, and the browser and device tabs are not built; the
pull request list page keeps its one panel beside the list.
The Connections page has a T3 Connect section (`connect_page.rs`, `t3_client::connect`): the environment's link read from
`GET /api/connect/link-state` when the page opens, when the window is focused again (a read newer than five seconds is kept) and
after each change; switches for the managed tunnel (desktop only), for publishing agent activity and for holding webhooks while
the environment is offline (managed links only), each gated on `relay:write` and on what the change needs; and the relay client's
status with its installation (`cloud.getRelayClientStatus`, `cloud.installRelayClient`), confirmed first and followed step by step.
The original links an environment with the account sign-in of the hosted service (Clerk) in the browser; this app has none, so it
does not link: turning the link off and changing the preferences of an existing link work, and what needs the account token (linking,
changing between a managed tunnel and publishing alone) is described, not offered. Link an environment with `t3 connect`.
A pairing link made in the Authorized clients list can be shared: the panel offers the endpoint to reach the machine at, the link
and a QR code (`t3_client::qr`, `qrcodegen`, the same generator as the original's `qrCode.ts`, compared by hash with its output).
Pairing credentials exchange into bearer sessions;
saved connections can be forgotten. The desktop UI defaults to the Rust server's
port 3774, with `T3_SERVER_URL` available for another existing server. Mobile
starts with a remote address form; native connection forms accept pairing
credentials without requiring manual HTTP requests. It uses the
original Effect RPC envelopes and validates
destination-specific permissions and environment identity before issuing actions.
Client tests cover history merging, stream cancellation, malformed-message
rollback, heartbeat policy, cancelled detail reads, and isolated environment state.
A rendering test verifies that identical thread/item IDs on another environment
receive fresh detail state. Another rendering test verifies that typing
does not rerender the project list or a 2,000-item conversation. Disconnections
trigger fresh session/ticket handshakes and subscriptions; uncertain mutations
are not replayed. In-process UI tests exercise the actual native HTTP and
WebSocket server for one-time pairing, project/thread commands, live updates,
read-only permissions, connection removal, and reconnect with an unsent draft.
Pending send receipts retain their original environment/thread ownership.
A deterministic provider subprocess test covers discovery, thread launch,
message/tool streaming, approval, user input, and interruption through the
actual UI transport. Reconnect during an active provider turn retains its session and unsent draft.
Live provider accounts still require integrated verification.

Existing-thread model, provider options, and permissions are composer draft choices.
They survive navigation and reload; sending first awaits a changed runtime mode,
then dispatches the selected model with the message. Accepted sends clear text
while retaining these choices. Tests cover provider-owned reported options,
started-session transition restrictions, model switching through actual provider
requests, and destination changes while settings requests are pending.
Draft recovery reads the original version-9 composer store without changing its
bytes; supported edits use a Rust sidecar. Unsupported saved context holds
the draft until the full composer is implemented; saved attachments no longer do. The original
shared-store writer and multiple local drafts remain unfinished.
Composer attachments (thread and new-thread composers) are added by file picker,
paste or drop, upload at once through `attachments.createUploadUrl` and the signed
URL, and are claimed by the send. Limits and messages follow the original; uploads
that finish after a thread switch stay with their own draft. Ready uploads persist
in the Rust sidecar and are verified on reload (an expired one asks to be attached
again); images and files saved by the original app load and re-upload instead of
holding the draft. Sent attachments render as fixed-aspect thumbnails and download
links from signed asset URLs. Not ported: image compression, HEIC conversion,
pasted-text folding, inline attachment chips, upload percentage, and the in-app
file viewer. Paste needs the webview bridge and a real window to verify.
The desktop CloseRequested hook queues the latest serialized draft write and
waits for its receipt before closing. Its compiled hook and writer tests pass;
actual native-window interaction, separate OS quit, and mobile pause are still
unverified or unimplemented.

The server now has an executable with HTTP authentication and WebSocket transport.
Configured Codex instances support text turns, streamed messages, reasoning,
plans and tools (commands, file changes, web search, MCP and dynamic tools with
their presentation), live command, file-change, permission and MCP-elicitation
approvals and input, token usage, retries and usage-limit stops, `/compact`, and
interruption. The Codex adapter is checked against the original's recorded
app-server transcripts (`tests/codex_replay.rs`); subagents, thread forking,
handoff and the ChatGPT-specific pieces are not ported. Native local ACP v1/v2 instances support
negotiated sessions, saved-session replay, assistant/reasoning text, plans, tools
and MCP presentation, live approvals, client filesystem and owned terminal
callbacks, model/config changes, interruption and durable recovery. Registry raw,
archive, npm and uv distributions can be installed. Scoped ACP authentication RPCs
support browser, environment-credential and owned-terminal interactions, with
owner privacy, cancellation, logout cleanup and provider-health refresh. Mounted
Rust Account controls test these flows through native RPCs, including permission
revocation and connection replacement. Exact source policy tests cover scalar
Unicode strings; seven isolated UTF-16 slice witnesses remain a compatibility gap.
Provider refresh and one-click updates, managed Codex installation (checksummed
official release, local `codex` preferred) and ACP registry session, provider and
logout management are served over the socket. Importing a native ACP session still needs
orchestration support for threads that own a native-session reference; ChatGPT-signed-in
managed Codex sessions (the sign-in and the profile transfer exist; a session does not run on the tokens), other provider authentication,
coordinator policy, handoff and checkpoint parity remain incomplete. Provider
sessions receive thread-scoped MCP credentials before startup, with native
stdio/HTTP transport and authenticated device tools. External MCP authentication
and OAuth challenges, other toolkits, negotiated ACP callback transport and Windows
runtime verification remain incomplete.
Other provider adapters, complete orchestration and filesystem/terminal services,
desktop/mobile native
integrations, and most interface features still require implementation. Thread creation requires a compatible server with configured
providers; unavailable services are shown as errors rather than simulated data.
Original TypeScript tests passing does not establish Rust feature parity.

Theme controls support persisted mode and mixed palettes, T3 Code and VS Code JSON import,
create/edit/duplicate/download, and lossless recovery of unknown library records.
File selection bounds reads, pairs light/dark families in batches, and preserves
the current selection for batch installs and update/copy conflict resolution.
Color conversion and palette generation pass the same original witnesses on host
and actual Rust WASM. The App-owned editor retains drafts across navigation; save and removal
transactions preserve mixed selections and report failed writes. Open VSX search and
package import validate licenses, checksums, archive bounds and theme includes;
collection updates require confirmation and compare fresh saved state after the
download. Closing a download cancels its body without changing the library.
Collection cards retain the selected variant through package updates and removals.
The global editor supports grouped advanced fields, search, usage highlighting and
interface inspection, with cleanup on close. Integrated browser checks cover these
flows; the source color picker, movable/minimized editor and complete visual fidelity
remain unfinished, including responsive color-field sizing. Unpaired UTF-16 string
boundaries and JSONC nesting beyond the current safe parser bound remain gaps.
The server's environment theme store publishes live (see above).
Native/mobile interaction remains unverified.

The intended UI targets are Dioxus WebAssembly web plus native desktop/mobile
builds sharing the Rust component and client layers. The WASM target check,
macOS desktop compilation, and Dioxus 0.7.10 web bundle build have passed:

```sh
# Before UI checks/builds (all WebViews load this Rust WASM terminal surface):
# Install the wasm-bindgen CLI at the exact version in rust/Cargo.lock.
# Generated assets are ignored; source, fonts and pinned Ghostty dependencies
# are committed under rust/crates/terminal and do not need the original app.
T3_WASM_BINDGEN=/path/to/wasm-bindgen rust/tools/build_terminal_surface.sh
cargo check --locked --manifest-path rust/Cargo.toml -p t3-ui --target wasm32-unknown-unknown -j 2
cargo check --locked --manifest-path rust/Cargo.toml -p t3-ui --no-default-features --features desktop -j 2
# From rust/crates/ui, with Dioxus CLI 0.7.10 and the WASM target installed:
CARGO_BUILD_JOBS=2 dx build --web --release --wasm-js-cfg=false --cargo-args='-j 2'
```

The optimized web bundle is written to `rust/target/dx/t3-ui/release/web/public`
and can be served with the native server's `--assets` option. Use the release
build for preview: the current debug WASM exceeds the static asset size limit.
A Chrome smoke pass verified same-origin pairing, cookie persistence on reload,
opening a thread, and sending a prompt through completed tool and assistant
output. Full visual fidelity, other browser flows, native desktop runtime, and
mobile builds have not yet been verified.
See the [Dioxus setup guide](https://dioxuslabs.com/learn/0.7/getting_started/)
for target tooling and native platform dependencies.

## Run the desktop app

The desktop app is the Dioxus UI (`t3-ui`, feature `desktop`) in a native window. It starts its own `t3-server` (the one
beside it in the bundle, or the one named by `T3_SERVER_BIN`), pairs with it, and stops it on quit. Closing the window on
macOS only hides it: the app and its server keep running, and a click on the dock icon shows the window again; elsewhere
closing the window quits.

```sh
CARGO_TARGET_DIR=<ui target> cargo build -p t3-ui --no-default-features --features desktop
CARGO_TARGET_DIR=<server target> cargo build -p t3-server
rust/tools/bundle_desktop_macos.sh --ui <ui target>/debug/t3-ui --server <server target>/debug/t3-server --out <dir>
open "<dir>/T3 Code.app"        # or run <dir>/T3 Code.app/Contents/MacOS/t3-ui from a terminal
```

| Variable               | Effect                                                                                                                                                                                                                           |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `T3_UI_DATA_DIR`       | Where the app keeps everything: the server's state (`server/`), its log (`logs/server.log`), the window's saved place, the web view's storage and `server.pid`. Default `~/Library/Application Support/T3 Code (Rust)` on macOS. |
| `T3_DESKTOP_STATE_DIR` | Moves only the server's state directory. It is refused when it is inside `~/.t3`, the original app's live data.                                                                                                                  |
| `T3_SERVER_BIN`        | The server program, instead of `t3-server` beside the app.                                                                                                                                                                       |
| `T3_SERVER_URL`        | Use this existing server and start none.                                                                                                                                                                                         |

The app learns that the server is ready from the `T3_SERVER_READY {json}` line the server prints on standard output once it is
bound and every service runs (see the process contract above), not by asking it; the server's output, with its error output, is
copied to `logs/server.log` as before. A server that prints no such line (the original's) is also asked over HTTP once three
seconds have passed, and a start that is neither announced nor answering within the supervisor's sixty seconds is ended and
tried again. `server.pid` is removed when the server has stopped on quit. When the server stops with the failure of the import
of the original app's data (`GET /api/rust/state-import`), the window shows the import-failed screen, and "Try again" there starts
the server again. The Connections page's T3 Connect section, relay client row and share panel are described in the UI list above:
the share panel (QR code and the choice among advertised endpoints) offers the one endpoint this app has, the server's own loopback
address, which gets no code because a device that scanned it would dial itself; LAN and Tailscale endpoints (the original's
`getAdvertisedEndpoints`) need a server that listens beyond loopback, which this app does not start.

Context menus (`contextMenu.show` in the original) are one model, `t3_client::context_menu` (the original's `ContextMenuItem`, and what its
native menu keeps of an item: headers dropped, empty submenus dropped, dividers placed as `ElectronMenu.ts` places them), with two
renderers behind `show(items, position)`: the page draws the menu itself (`context_menu.rs`; submenus, option rows, headers, keyboard
and outside-click dismissal, one menu at a time), and the desktop app on macOS shows the system's own menu at the pointer
(`desktop_shell::native_menu`, scaled by the page zoom). Elsewhere on the desktop, where no native menu is built yet (Windows, Linux), the page's
menu is shown. Callers: the thread rows (one menu for the row, another for several chosen rows), a link in a message, a file named in a message, a
changed file in the diff panel and the timeline, a pull request's number, a branch name, a tab of the panel beside the conversation, an archived thread, and a
picture or video in a message. The original's other callers have no counterpart here yet: the terminal drawer (the terminal surface has no
paste entry in its ABI), the file browser tree, the run-context control, and draft rows. Not shown on the native
menu: the destructive item's trash icon, and the icons the page draws.

The server is started with the environment of your login shell (`$SHELL -ilc`, then `/bin/zsh` or `/bin/bash`; `launchctl getenv
PATH` on macOS as a last resort), so provider tools on your `PATH` are found when the app is opened from the Finder. The shell is
asked once per run, for at most five seconds. Windows is not covered yet: the server cannot be started there by this app.

## HTML renders

`html_render` and `html_preview` run (`mcp_html.rs`, `html_render.rs`). A page's local images are inlined, the theme bootstrap is injected, and the pinned headless shell renders and measures it over the DevTools protocol (`cdp.rs`, `headless_chrome.rs`: pipe or loopback WebSocket). The browser's every connection goes through a SOCKS5 proxy that only reaches public addresses (`public_proxy.rs`), slightly stricter than the original's (it also refuses IPv4-translated and site-local IPv6 and the IETF, documentation and 6to4-relay IPv4 ranges). A published page is removed with its thread. Unix only; the host-setup command it names (`browser setup`) is not built.

## Several environments

The window keeps one connection per environment (`crates/ui/src/links.rs`). The open environment's state is the model's flat fields; every other connected environment's is parked in `UiModel::parked` and keeps receiving its shell and configuration. Switching swaps the two without asking any server for anything but the open thread. A supervisor per link reconnects with the original's backoff (`t3_client::supervisor`), tries the environment's other routes first, and can be told to retry now. The sidebar's projects and threads, the project filter, thread search, the command palette's threads and projects, thread navigation keys and notifications read every connected environment (`fleet.rs`): rows are keyed `(environment, id)`, projects group across environments as the original's do, and opening a row of another environment moves the window to it (`select_thread_in`). A write to such a row goes out on that environment's own link; what needs the window's own dialogs (archive, delete, unpin, custom snooze, project settings) moves the window first. Rows of a group that spans environments, or of a parked environment, are not draggable. The new-thread form's 'Run on' choice (and `composer.host`, `composer.cycleHost`) moves the form and its message to another environment's project of the same repository; 'Auto balance' (Connections, Load balancing) picks the machine with the most free CPU and memory from `server.getHostResources`, once, when chosen. A remote environment's managed Codex signs in to ChatGPT through a connected local environment (`chatgpt_handoff.rs`); the web build can receive the redirect through that environment. The welcome wizard sets up every ticked connected computer in turn. Not built: GitHub routing between environments; drafts, archived lists and bulk selection span the open environment only.

Saved connections are the original's `ConnectionCatalogDocument` (`t3_client::catalog`). Web: `localStorage` key `t3code:rust-connection-catalog:v1`, plaintext, as the original's IndexedDB document is. Desktop: `<data dir>/connection-catalog.json`, AES-256-GCM, key in the macOS keychain (service `T3 Code (Rust) saved connections`) or, when `T3_UI_DATA_DIR` is set, in `connection-catalog.key` beside it (mode 0600, no protection against the same user). Other desktop platforms save nothing.

## Pull request sync on request, managed Codex text, runtime lease

Pull request sync (`pull_request_sync.rs`) also reads a pull request at once when asked (`request_sync`: after `pullRequests.runAction` and `pullRequests.invalidate`, when a reading sees a state change, when a link with no snapshot is added, and when a run that ran `gh pr merge` ends). A rate-limited host is paused per project until the time it names. Text generation on a managed Codex runs on the signed-in tokens under the sign-in's admission. The managed runtime holds a lease (`CodexInstallation.acquire`): it cannot be removed while a session, probe or text run uses it.

## Server browser (off by default)

Set `T3CODE_RUST_SERVER_BROWSER=1` to run the environment's own headless browser. Agents' preview tabs open in isolated in-memory contexts of one shared Chromium, people's tabs in persistent per-profile browsers under `<state>/server-browser/profiles`, and viewers watch and take over through `/api/preview-stream/*`. With the switch off the server starts no browser, does not advertise `serverBrowser`, and those routes answer 404. The browser has no network policy yet: tabs reach whatever the machine reaches, and `preview_upload` accepts any absolute path, as in the original (`TODO(original …)` markers name each place). It has only been run against a scripted DevTools endpoint, never a real Chrome. `t3 browser setup` prepares a Linux host (AppArmor profile for Chrome's sandbox, Debian libraries); it needs root and prints the `sudo` line otherwise.
