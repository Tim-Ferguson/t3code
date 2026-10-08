The `cbrt` function in `crates/client/src/themes/color.rs` is a Rust translation of
V8 12.9.202's `src/base/ieee754.cc` implementation:
https://github.com/v8/v8/blob/12.9.202/src/base/ieee754.cc

It retains the source operation order and binary rounding because the original
Node color converter uses this algorithm. `NOTICE` preserves the fdlibm/Sun and
V8 source notices; `LICENSE` is the pinned V8 distribution license. No V8 runtime
is included.
