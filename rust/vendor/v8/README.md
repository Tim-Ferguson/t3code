The `cbrt` function in `crates/client/src/themes/color.rs` is a Rust translation of
V8 12.9.202's `src/base/ieee754.cc` implementation:
https://github.com/v8/v8/blob/12.9.202/src/base/ieee754.cc

It retains the source operation order and binary rounding because the original
Node color converter uses this algorithm. `NOTICE` preserves the fdlibm/Sun and
V8 source notices; `LICENSE` is the pinned V8 distribution license. No V8 runtime
is included.

Color fixture regeneration is pinned to Node 23.11.0 / V8 12.9.202.28-node.14,
captured on macOS arm64. Node 24.13.1 changes seven extreme-magnitude results, so
its output is a separate cross-runtime comparison rather than a replacement
corpus. The Node WASM verifier may run on Node 24: numeric operations execute in
the Rust WASM module, against the committed pinned fixtures.
