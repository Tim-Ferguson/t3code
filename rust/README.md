# T3 Code Rust port

This directory contains the work-in-progress Rust replacement. The original
application remains outside this directory as the behavior and test reference.
The current Rust implementation is a foundation, not a feature-complete replacement.

From the repository root:

```sh
cargo test --locked --workspace --manifest-path rust/Cargo.toml
cargo check --locked --manifest-path rust/Cargo.toml -p t3-ui
```

The workspace contains shared JSON contracts (`t3-contracts`), client connection,
RPC and projection state (`t3-client`), SQLite event/receipt/outbox persistence and
project commands and HTTP/WebSocket transport (`t3-server`), and a Dioxus UI (`t3-ui`). Contracts
preserve unported nested payloads as JSON. Compatibility tests cover legacy model
selection, provider slugs, authentication grants, forward event decoding, and
Effect RPC envelopes. Server tests cover atomic commits, durable command receipts,
project validation, and effect leases.

The UI implements the project/thread sidebar, conversation and composer,
approval and user-input responses, remote server connections, provider listing,
and theme selection. It uses the original Effect RPC envelopes and validates
destination-specific permissions and environment identity before issuing actions.
Client tests cover history merging, stream cancellation, malformed-message
rollback, and isolated environment state. A rendering test verifies that typing
does not rerender the project list or a 2,000-item conversation.

The server now has an executable with HTTP authentication and WebSocket transport.
Provider execution, complete orchestration, filesystem and terminal services,
desktop/mobile native integrations, and most interface features still require
implementation. Thread creation requires a compatible server with configured
providers; unavailable services are shown as errors rather than simulated data.
Original TypeScript tests passing does not establish Rust feature parity.

The intended UI targets are Dioxus WebAssembly web plus native desktop/mobile
builds sharing the Rust component and client layers. The WASM target check,
macOS desktop compilation, and Dioxus 0.7.10 web bundle build have passed:

```sh
cargo check --locked --manifest-path rust/Cargo.toml -p t3-ui --target wasm32-unknown-unknown -j 2
cargo check --locked --manifest-path rust/Cargo.toml -p t3-ui --no-default-features --features desktop -j 2
# From rust/crates/ui, with Dioxus CLI 0.7.10 and the WASM target installed:
CARGO_BUILD_JOBS=2 dx build --web --wasm-js-cfg=false --cargo-args='-j 2'
```

The web bundle is written to `rust/target/dx/t3-ui/debug/web/public`.
Browser integration and visual fidelity have not yet been verified. Native
desktop runtime and mobile builds have not yet been verified.
See the [Dioxus setup guide](https://dioxuslabs.com/learn/0.7/getting_started/)
for target tooling and native platform dependencies.
