The native scanner sources in `oniguruma/` are unmodified Oniguruma 6.9.8
from commit `08d36110c5670c815ad6d6f969e578049d209080` of
https://github.com/kkos/oniguruma. The upstream license is `oniguruma/COPYING`.
`oniguruma/source-hashes.json` records the original source bytes.

That is the submodule revision used by Microsoft's vscode-oniguruma v1.7.0
(commit `716aeaa229e4ae2e3b0057377b55743e9a3e995b`). The original browser binary from npm vscode-oniguruma 1.7.0 is 466,610 bytes,
SHA-256 `fd885c2d12e5951e59d761ebd4a006e06254b1491fd6f530c92b69fb4d8d77d9`,
byte-identical to the original app's @shikijs/engine-oniguruma 4.2.0 dependency.
The Rust app ships a rebuilt 463,994-byte scanner dependency instead. The pinned
wrapper source and `safe-constructor.patch` are in `vscode-oniguruma/`; the patch
checks compile failures before dereferencing and correctly frees prior regexes.
Malformed patterns now return errors deliberately, rather than reproducing the
upstream undefined-memory behavior. Valid source behavior is tested separately.

`../assets/onig-build.json` records source/compiler/download/output hashes.
`rust/tools/rebuild_oniguruma_wasm.sh` reproduces the build with Emscripten 3.1.73
ARM64 (the upstream historical build used 3.1.21). Two builds in separate output
directories produced byte-identical WASM. The application uses only that WASM
scanner dependency and its Rust ABI adapter, not the generated JavaScript.
The wrapper license is `../assets/VSCODE-ONIGURUMA-LICENSE.txt`.

The adapter and tokenizer are Rust port code. These upstream regex engines
are dependencies; no Shiki or vscode-textmate JavaScript runtime ships in
the Rust application. Development fixture generation executes the pinned
original implementations to measure behavior.

The Rust TextMate rule/theme behavior is derived from the pinned
`@shikijs/vscode-textmate` 10.0.2 fork, with Microsoft's MIT notice in
`TEXTMATE-LICENSE.txt`. The initial grammar fixture data comes from
`@shikijs/langs` 4.2.0 (`SHIKI-LANGUAGES-LICENSE.txt`). The Pierre light/dark
fixture theme data is `@pierre/theme` 2.0.0; its Apache license and inherited
Primer attribution are `PIERRE-THEME-LICENSE.txt` and `PIERRE-THEME-NOTICE.md`.
