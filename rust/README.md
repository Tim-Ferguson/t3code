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
Browser IPC and OTLP requests currently fail explicitly.
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
Device-host settings resolve SSH aliases without connecting, exclude only self
targets, and preserve proxies, forwarded ports and unresolved entries. Local device
state and session lifecycle, consent configuration and agent-daemon bootstrap,
reuse and shutdown are implemented. Other toolkit injection pipelines and remote
SSH hosts remain unfinished; the guarded launcher uses an externally installed
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
the settings secrets are used in place when the state directory is the original's, and are
copied otherwise (settings load-time migrations then run on first start as ever). The import
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
deleted (its scan cache) is not carried. What the import serves is held to what the original
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
default `~/.gemini/antigravity*` directories and each Antigravity instance's profile), opened read-only on every scan, with the
original's alias merging; `tests/usage_readers_flow.rs` replays what the unchanged readers made of the same stores
(`rust/tools/generate_usage_reader_fixtures.mjs`). Grok and Cursor are still not read. The pull request sync has a flow test
(`tests/pull_request_sync_flow.rs`) over a fake GitHub.

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
and lists the environment's pull requests.
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
managed Codex sessions, ChatGPT profile transfer, other provider authentication,
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
Live environment theme publication depends on the unfinished backend theme store.
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

The server is started with the environment of your login shell (`$SHELL -ilc`, then `/bin/zsh` or `/bin/bash`; `launchctl getenv
PATH` on macOS as a last resort), so provider tools on your `PATH` are found when the app is opened from the Finder. The shell is
asked once per run, for at most five seconds. Windows is not covered yet: the server cannot be started there by this app.
