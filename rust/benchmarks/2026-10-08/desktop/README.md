# Desktop visibility lifecycle benchmark — 2026-10-08

Measured on Apple M4, 32 GiB RAM, 10 logical CPUs, macOS 27.0, arm64.
Both real apps passed parent-operated CUA pilots before timing. The benchmark
used the original production Electron bundles and an optimized Rust release
bundle based on commit `fcd48c83a4aa9fa7d79ccd3336f43256c8eb4fa7`.
The Rust port remains partial and is not feature-equivalent to the original.

| Profile       | App      | Launches |       Median | p95 nearest rank |
| ------------- | -------- | -------: | -----------: | ---------------: |
| Warm          | Original |       10 | 1,968.996 ms |     2,441.521 ms |
| Warm          | Rust     |       10 | 1,191.848 ms |     1,251.556 ms |
| Fresh profile | Original |       10 | 1,734.680 ms |     2,449.107 ms |
| Fresh profile | Rust     |       10 |   771.094 ms |     1,932.580 ms |

The warm median ratio is 1.652× (original/Rust). This measures **parent process
spawn to a native visibility lifecycle marker**, not first paint, usable UI,
backend readiness or total application startup. Original marks the main
`t3code://app/` window's `show` event; Rust marks return from native
`set_visible` after Dioxus has queued its initial DOM edits. Their rendering
pipelines and implemented startup work differ. These observations cannot isolate
language effects or demonstrate finished-app performance.

Both launches were UI-only: original local environment disabled using its real
persisted setting, Rust `T3_SERVER_URL` empty, no saved connections or owned
backend startup. Orders alternated each round. Warm mode excluded one untimed
setup launch per app, then reused its isolated profile. Fresh mode used a new
profile for every launch; OS/filesystem caches were not purged. All 40 measured
launches and both warmups completed, with every observed descendant exiting.
Only captured direct child PIDs were signaled; shared/OS-owned XPC services were
not terminated. Other user/preview apps were untouched.

## Evidence

- `comparison.json` / `comparison.md`: summaries, source/artifact hashes and raw
  observations, produced by `write-desktop-report.mjs`.
- `warm-results.json`, `fresh-profile-results.json`: complete results including
  untimed warmups, host/UTC, captured PID markers, WebKit UUIDs and cleanup.
- `*-samples.jsonl`: all raw rows, explicitly labeled `measured`.
- `sanitization.json`: original/retained file hashes and path alias definitions.
  Absolute checkout/preparation/profile paths were replaced with `${...}` aliases;
  numeric data, timestamps, process IDs, UUIDs and artifact hashes are unchanged.
  No stderr, credentials, application databases or profile caches are retained.
- `rust-desktop-artifact.json`: 22,193,776-byte optimized arm64 executable,
  bundle ID and SHA256. Build completed in 278.94 s after copying the exact
  generated terminal assets into the committed snapshot.

## Exact measured source

`measured/*.txt` preserves the three runtime harness scripts and benchmark Rust
entry byte-for-byte, outside formatter extensions. `measured/manifest.json`
records SHA256; the three script hashes match the raw launch metadata. Runnable
script copies may receive formatting later; copy the exact retained sources back
to their original filenames together to reproduce the measured harness bytes.

## Reproduction

Use installed dependencies and Node 24.13.1. From a checkout of the stated
revision, build the original production bundles with dev server variables unset:

```sh
T3CODE_WEB_SOURCEMAP=false node_modules/.bin/vp run build:desktop
```

This task builds web, server/client assets and Electron boot/main/preload. It does
not launch Electron unless `T3CODE_DESKTOP_DEV=1`. The unpackaged stock Electron
binary is `apps/desktop/node_modules/electron/dist/Electron.app/Contents/MacOS/Electron`.
`original-desktop-bootstrap.cjs` overrides Node's `os.homedir`/`os.tmpdir` before
original imports, sets an isolated `T3CODE_HOME` and Chromium profile, disables
updates and checks built assets. It never reassigns `HOME`. The launcher flag
`--t3code-dev-root` is an identity tag, not an asset override: unpackaged app
assets resolve from the required production `main.cjs` directory.

Prepare the Rust committed snapshot with Python >=3.12, without editing the
active checkout or cargo cache:

```sh
python3 prepare-rust-snapshot.py /private/tmp/t3port-bench-source-repro \
  /private/tmp/t3port-bench-dioxus-repro \
  --bundle-identifier org.t3port.benchmark.reproUniqueSuffix
```

The helper validates pinned cached Dioxus source hashes and applies the 2 KiB
patch only to its copied dependency. Dioxus is MIT/Apache-2.0; license notices
are retained. `dioxus-benchmark-patch-manifest.json` records version/provenance;
`dioxus-crate-hashes.json` records original and patched package file hashes.
No upstream vendor tree is retained here.

Generate the committed Rust terminal surface assets with the snapshot's
`rust/tools/build_terminal_surface.sh` (its prerequisites are documented in
`rust/README.md`). The measured build reused byte-identical generated assets;
`terminal-surface-hashes.json` records them. Resolve the copied dependency in the
snapshot with full offline Cargo metadata, then verify locked metadata. Build
from snapshot `rust/crates/ui` using dx 0.7.10:

```sh
CARGO_TARGET_DIR=<shared-target> CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 \
  dx build --desktop --release --no-default-features --features desktop \
  --locked --offline --session-cache-dir /private/tmp/t3port-bench-dx-repro \
  --cargo-args=-j2
```

The CLI field is `[bundle] identifier`, verified by the pinned CLI's JSON schema
and resulting Info.plist. Use a unique identifier and verify the bundled
executable/identifier before launch. WKWebView ignores `with_data_directory` on
macOS; the benchmark-only patch requires an explicit UUID through Wry's supported
`with_data_store_identifier` (macOS >=14). Warm trials reuse one UUID, fresh
trials use distinct UUIDs. No default/preview WebKit store or guessed filesystem
cleanup is used. Persistent benchmark-owned stores remain until explicitly
removed using WebKit's supported API.

Replace aliases in the retained configs with current absolute executable/script
paths, and use new `/private/tmp/t3port-bench-*` roots. Perform resident pilots
with `launch-desktop-pilot.mjs`, checking the actual apps through CUA. Stop only
the captured launchers. Then, with all builds and other benchmark runs idle:

```sh
node run-desktop-lifecycle.mjs <warm-config.json> --run
node run-desktop-lifecycle.mjs <fresh-profile-config.json> --run
node write-desktop-report.mjs <warm-root>/results.json \
  <fresh-root>/results.json /private/tmp/t3port-bench-desktop-comparison
```

The runner records host/UTC, artifact and harness hashes, raw markers and cleanup.
The report writer validates 10 complete rounds per app, warmup exclusion, exact
captured-PID markers, profile/UUID reuse, successful cleanup and matching hosts,
artifacts and milestones. It recomputes medians and p95 from measured rows.
No UI/DOM/AX polling or synthetic input is part of the timing harness.
