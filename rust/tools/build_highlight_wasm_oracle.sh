#!/usr/bin/env bash
set -euo pipefail
HIGHLIGHT_RUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
HIGHLIGHT_BINDGEN="${T3_WASM_BINDGEN:-$(command -v wasm-bindgen || true)}"
if [[ -z "$HIGHLIGHT_BINDGEN" ]]; then
  echo 'Set T3_WASM_BINDGEN to the wasm-bindgen CLI matching Cargo.lock.' >&2
  exit 1
fi
HIGHLIGHT_VERSION="$(python3 - "$HIGHLIGHT_RUST_DIR/Cargo.lock" <<'PY'
import sys,tomllib
lock=tomllib.load(open(sys.argv[1],'rb'))
print(next(p['version'] for p in lock['package'] if p['name']=='wasm-bindgen'))
PY
)"
if [[ "$("$HIGHLIGHT_BINDGEN" --version)" != "wasm-bindgen $HIGHLIGHT_VERSION" ]]; then
  echo "wasm-bindgen CLI must match locked version $HIGHLIGHT_VERSION." >&2
  exit 1
fi
python3 - "$HIGHLIGHT_RUST_DIR" "$HIGHLIGHT_VERSION" <<'PY'
import json,pathlib,shutil,sys
root=pathlib.Path(sys.argv[1]);directory=root/'target/highlight-oracle-manifest';directory.mkdir(parents=True,exist_ok=True)
quote=lambda path:json.dumps(str(path))
(directory/'Cargo.toml').write_text('[package]\nname="t3-highlight-wasm-oracle"\nversion="0.0.0"\nedition="2024"\n[workspace]\n[lib]\ncrate-type=["cdylib"]\npath='+quote(root/'tools/highlight_wasm_oracle.rs')+'\n[dependencies]\nt3-highlight={path='+quote(root/'crates/highlight')+'}\nserde_json={version="1",features=["preserve_order","float_roundtrip"]}\nwasm-bindgen="='+sys.argv[2]+'"\nwasm-bindgen-futures="0.4"\n')
shutil.copyfile(root/'Cargo.lock',directory/'Cargo.lock')
PY
# The temporary manifest keeps oracle ABI exports and package membership out of production.
cargo metadata --manifest-path "$HIGHLIGHT_RUST_DIR/target/highlight-oracle-manifest/Cargo.toml" --offline --format-version 1 > "$HIGHLIGHT_RUST_DIR/target/highlight-oracle-manifest/metadata.json"
cargo build --manifest-path "$HIGHLIGHT_RUST_DIR/target/highlight-oracle-manifest/Cargo.toml" --target-dir "$HIGHLIGHT_RUST_DIR/target" --target wasm32-unknown-unknown --release --offline --locked -j2
"$HIGHLIGHT_BINDGEN" "$HIGHLIGHT_RUST_DIR/target/wasm32-unknown-unknown/release/t3_highlight_wasm_oracle.wasm" --target nodejs --out-dir "$HIGHLIGHT_RUST_DIR/target/highlight-oracle-node"
printf '%s\n' '{"type":"commonjs"}' > "$HIGHLIGHT_RUST_DIR/target/highlight-oracle-node/package.json"
