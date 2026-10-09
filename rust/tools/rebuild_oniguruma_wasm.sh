#!/usr/bin/env bash
# Rebuild the pinned scanner dependency; no grammar/tokenizer JavaScript ships.
set -euo pipefail
HIGHLIGHT_RUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"
HIGHLIGHT_EMCC="${T3_EMCC:?Set T3_EMCC to Emscripten 3.1.73 emcc}"
HIGHLIGHT_OUT="${T3_ONIG_BUILD_DIR:-/private/tmp/t3port-onig-wasm-rebuild}"
mkdir -p "$HIGHLIGHT_OUT"
python3 - "$HIGHLIGHT_RUST_DIR" "$HIGHLIGHT_OUT" <<'PY'
import hashlib,json,pathlib,re,sys
root=pathlib.Path(sys.argv[1]);out=pathlib.Path(sys.argv[2]);crate=root/'crates/highlight'
bridge=crate/'vendor/vscode-oniguruma'
expected=json.loads((bridge/'source-hashes.json').read_text())['onig.cc']
assert hashlib.sha256((bridge/'onig.cc').read_bytes()).hexdigest()==expected
for entry in json.loads((crate/'vendor/oniguruma/source-hashes.json').read_text()):
    name=entry['path'];expected=entry['sha256']
    assert hashlib.sha256((crate/'vendor/oniguruma'/name).read_bytes()).hexdigest()==expected,name
(out/'onig.cc').write_bytes((bridge/'onig.cc').read_bytes())
(out/'config.h').write_text('#define HAVE_STDINT_H 1\n#define HAVE_INTTYPES_H 1\n#define HAVE_SYS_TYPES_H 1\n#define SIZEOF_INT 4\n#define SIZEOF_LONG 4\n#define SIZEOF_LONG_LONG 8\n#define SIZEOF_VOIDP 4\n#define HAVE_UNISTD_H 1\n#define HAVE_SYS_TIME_H 1\n#define HAVE_SYS_TIMES_H 1\n#define HAVE_ALLOCA_H 1\n')
# Keep the native and browser builds on the same pinned translation units.
source=(crate/'build.rs').read_text().split('let files = [',1)[1].split('];',1)[0]
(out/'sources.txt').write_text('\n'.join(str(crate/'vendor/oniguruma/src'/f'{name}.c') for name in re.findall(r'"([a-z0-9_]+)"',source))+'\n')
PY
(cd "$HIGHLIGHT_OUT" && patch --batch -p0 < "$HIGHLIGHT_RUST_DIR/crates/highlight/vendor/vscode-oniguruma/safe-constructor.patch")
# Arrays preserve paths without shell interpolation. Emscripten's internal
# compile parallelism is bounded independently of Cargo.
HIGHLIGHT_SOURCES=()
while IFS= read -r HIGHLIGHT_SOURCE; do HIGHLIGHT_SOURCES+=("$HIGHLIGHT_SOURCE"); done < "$HIGHLIGHT_OUT/sources.txt"
EMCC_CORES=2 "$HIGHLIGHT_EMCC" -O2 "${HIGHLIGHT_SOURCES[@]}" "$HIGHLIGHT_OUT/onig.cc" \
  -I"$HIGHLIGHT_OUT" -I"$HIGHLIGHT_RUST_DIR/crates/highlight/vendor/oniguruma/src" \
  -o "$HIGHLIGHT_OUT/onig.js" -s ENVIRONMENT=shell -s FILESYSTEM=0 \
  -s MODULARIZE=1 -s EXPORT_NAME=Onig -s ALLOW_MEMORY_GROWTH=1 \
  -s EXPORTED_RUNTIME_METHODS="['UTF8ToString']" -s DYNAMIC_EXECUTION=0
"$HIGHLIGHT_EMCC" --version > "$HIGHLIGHT_OUT/compiler-version.txt"
shasum -a 256 "$HIGHLIGHT_OUT/onig.wasm"
# Copy into the application only after exact valid behavior and intentional
# malformed-pattern rejection have been verified on this candidate.
