#!/usr/bin/env bash
set -euo pipefail
TERMINAL_RUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TERMINAL_BINDGEN="${T3_WASM_BINDGEN:-$(command -v wasm-bindgen || true)}"
if [[ -z "$TERMINAL_BINDGEN" ]]; then
  echo 'Set T3_WASM_BINDGEN to the wasm-bindgen CLI matching Cargo.lock.' >&2
  exit 1
fi
TERMINAL_EXPECTED_VERSION="$(python3 - "$TERMINAL_RUST_DIR/Cargo.lock" <<'PY'
import sys,tomllib
lock=tomllib.load(open(sys.argv[1],'rb'))
print(next(p['version'] for p in lock['package'] if p['name']=='wasm-bindgen'))
PY
)"
if [[ "$("$TERMINAL_BINDGEN" --version)" != "wasm-bindgen $TERMINAL_EXPECTED_VERSION" ]]; then
  echo "wasm-bindgen CLI must match locked version $TERMINAL_EXPECTED_VERSION." >&2
  exit 1
fi
cargo build --manifest-path "$TERMINAL_RUST_DIR/Cargo.toml" -p t3-terminal --target wasm32-unknown-unknown --release --offline --locked -j2
"$TERMINAL_BINDGEN" "$TERMINAL_RUST_DIR/target/wasm32-unknown-unknown/release/t3_terminal.wasm" --target web --out-dir "$TERMINAL_RUST_DIR/crates/ui/assets/terminal-surface"
"$TERMINAL_BINDGEN" "$TERMINAL_RUST_DIR/target/wasm32-unknown-unknown/release/t3_terminal.wasm" --target nodejs --out-dir "$TERMINAL_RUST_DIR/target/terminal-node"
printf '%s\n' '{"type":"commonjs"}' > "$TERMINAL_RUST_DIR/target/terminal-node/package.json"
