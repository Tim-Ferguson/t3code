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
Browser IPC, Tailscale Serve and OTLP requests currently fail explicitly.
Automatic browser launching remains unimplemented, so `noBrowser` has no
launcher to control. Windows inherited descriptors, original database/layout
migration and the desktop producer remain pending. Host resources are
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
disconnect removes only that connection's leases. Importing the original database
layout and periodic provider, VCS, usage and Git consumers remain pending.
Device-host settings resolve SSH aliases without connecting, exclude only self
targets, and preserve proxies, forwarded ports and unresolved entries. Full device
services, hub connections, SSH bootstrap and device RPCs remain unfinished.

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
listing, and theme selection. Pairing credentials exchange into bearer sessions;
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
bytes; supported edits use a Rust sidecar. Unsupported saved attachments and
context hold the draft until the full composer is implemented. The original
shared-store writer and multiple local drafts remain unfinished.
The desktop CloseRequested hook queues the latest serialized draft write and
waits for its receipt before closing. Its compiled hook and writer tests pass;
actual native-window interaction, separate OS quit, and mobile pause are still
unverified or unimplemented.

The server now has an executable with HTTP authentication and WebSocket transport.
Configured Codex instances support text turns, streamed messages/tools, live
approvals and input, and interruption. Native local ACP v1/v2 instances support
negotiated sessions, saved-session replay, assistant/reasoning text, plans, tools
and MCP presentation, live approvals, client filesystem and owned terminal
callbacks, model/config changes, interruption and durable recovery. Registry raw
binaries can be installed; other distributions, full registry/authentication and
coordinator policy, MCP injection, handoff and checkpoint parity remain incomplete.
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
