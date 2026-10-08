#!/usr/bin/env bash
set -euo pipefail
THEME_RUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
THEME_BINDGEN="${T3_WASM_BINDGEN:-$(command -v wasm-bindgen || true)}"
if [[ -z "$THEME_BINDGEN" ]]; then
  echo 'Set T3_WASM_BINDGEN to the wasm-bindgen CLI matching Cargo.lock.' >&2
  exit 1
fi
THEME_VERSION="$(python3 - "$THEME_RUST_DIR/Cargo.lock" <<'PY'
import sys,tomllib
lock=tomllib.load(open(sys.argv[1],'rb'))
print(next(p['version'] for p in lock['package'] if p['name']=='wasm-bindgen'))
PY
)"
if [[ "$("$THEME_BINDGEN" --version)" != "wasm-bindgen $THEME_VERSION" ]]; then
  echo "wasm-bindgen CLI must match locked version $THEME_VERSION." >&2
  exit 1
fi
python3 - "$THEME_RUST_DIR" "$THEME_VERSION" <<'PY'
import json,pathlib,shutil,sys
root=pathlib.Path(sys.argv[1]);directory=root/'target/theme-download-oracle-manifest';directory.mkdir(parents=True,exist_ok=True)
quote=lambda path:json.dumps(str(path))
(directory/'Cargo.toml').write_text('[package]\nname="t3-theme-download-wasm-oracle"\nversion="0.0.0"\nedition="2024"\n[workspace]\n[lib]\ncrate-type=["cdylib"]\npath='+quote(root/'tools/theme_download_wasm_oracle.rs')+'\n[dependencies]\nt3-client={path='+quote(root/'crates/client')+'}\nserde_json={version="1",features=["preserve_order","float_roundtrip"]}\nwasm-bindgen="='+sys.argv[2]+'"\n')
(directory/'Cargo.toml').write_text((directory/'Cargo.toml').read_text()+"futures-util=\"0.3\"\njs-sys=\"0.3\"\nwasm-bindgen-futures=\"0.4\"\ngloo-timers={version=\"0.3\",features=[\"futures\"]}\nweb-sys={version=\"0.3\",features=[\"AbortController\",\"AbortSignal\",\"RequestInit\",\"Response\",\"Headers\",\"ReadableStream\",\"ReadableStreamDefaultReader\"]}\n")
shutil.copyfile(root/'Cargo.lock',directory/'Cargo.lock')
PY
# The temporary manifest keeps oracle ABI exports and package membership out of production.
cargo metadata --manifest-path "$THEME_RUST_DIR/target/theme-download-oracle-manifest/Cargo.toml" --offline --format-version 1 > "$THEME_RUST_DIR/target/theme-download-oracle-manifest/metadata.json"
cargo build --manifest-path "$THEME_RUST_DIR/target/theme-download-oracle-manifest/Cargo.toml" --target-dir "$THEME_RUST_DIR/target" --target wasm32-unknown-unknown --release --offline --locked -j2
"$THEME_BINDGEN" "$THEME_RUST_DIR/target/wasm32-unknown-unknown/release/t3_theme_download_wasm_oracle.wasm" --target nodejs --out-dir "$THEME_RUST_DIR/target/theme-download-oracle-node"
printf '%s\n' '{"type":"commonjs"}' > "$THEME_RUST_DIR/target/theme-download-oracle-node/package.json"
