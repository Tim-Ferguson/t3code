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
project commands (`t3-server`), and a Dioxus UI entry point (`t3-ui`). Contracts
preserve unported nested payloads as JSON. Compatibility tests cover legacy model
selection, provider slugs, authentication grants, forward event decoding, and
Effect RPC envelopes. Server tests cover atomic commits, durable command receipts,
project validation, and effect leases.

The UI entry point currently renders a placeholder. The server is currently a
library and has no runnable HTTP/WebSocket binary. Provider adapters, complete
orchestration, authentication/session transport, filesystem and terminal services,
desktop/mobile shells, and the full interface still require implementation.
Original TypeScript tests passing does not establish Rust feature parity.

The intended UI targets are Dioxus WebAssembly web plus native desktop/mobile
builds sharing the Rust component and client layers. To serve the web entry point,
install the Dioxus CLI and the `wasm32-unknown-unknown` Rust target, then run
`dx serve --platform web` from `rust/crates/ui`. Platform UI builds and a browser
integration pass have not yet been verified.
See the [Dioxus setup guide](https://dioxuslabs.com/learn/0.7/getting_started/)
for target tooling and native platform dependencies.
