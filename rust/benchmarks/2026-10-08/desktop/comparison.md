# Desktop window visibility lifecycle benchmark

Generated 2026-10-08T21:05:11.791Z; source fcd48c83a4aa9fa7d79ccd3336f43256c8eb4fa7.

Metric: Parent process spawn to native visibility lifecycle marker receipt. This measures an instrumented native lifecycle stage, not first paint or usable UI.

Host: Mac; darwin 27.0 (27.0.0); arm64; Apple M4; 10 logical CPUs; 32.0 GiB RAM.

| Profile       | Runtime  |   n | Median ms |  p95 ms |  Min ms |  Max ms |
| ------------- | -------- | --: | --------: | ------: | ------: | ------: |
| Warm          | original |  10 |   1969.00 | 2441.52 | 1840.18 | 2441.52 |
| Warm          | rust     |  10 |   1191.85 | 1251.56 |  835.16 | 1251.56 |
| Fresh profile | original |  10 |   1734.68 | 2449.11 | 1539.74 | 2449.11 |
| Fresh profile | rust     |  10 |    771.09 | 1932.58 |  547.39 | 1932.58 |

Original marker: `electron-window-shown`. Rust marker: `rust-native-set-visible-completed`. Median is the midpoint of sorted observations5/6; p95 uses nearest rank (maximum for n=10).

Warm: Rust/original median ratio 0.605; Rust minus original -777.15 ms. Period 2026-10-08T21:02:48.143Z–2026-10-08T21:03:48.946Z.

Fresh profile: Rust/original median ratio 0.445; Rust minus original -963.59 ms. Period 2026-10-08T21:04:00.450Z–2026-10-08T21:04:52.071Z.

## Limits

- Native lifecycle marker, not first paint or usable UI
- Parent spawn to marker receipt includes OS startup and pipe delivery
- Original unpackaged production Electron bundles; Rust release bundle with benchmark-only entry/dependency patch
- Both disconnected UI-only with no owned backend startup
- Fresh-profile is not a cold OS/filesystem-cache measurement
- The source apps differ in implemented features and startup work; this comparison does not isolate language effects
- Visibility lifecycle does not prove first paint, usable UI, connection readiness or total startup
- Observed descendant cleanup excludes OS-owned/shared XPC processes; no unrelated process is signaled

## Provenance

- ${REPOSITORY}/apps/desktop/node_modules/electron/dist/Electron.app/Contents/MacOS/Electron: `ad321e34b2795d1910ff1a24f2b4249269f556dc7184ea64f353a851339f1582`
- ${REPOSITORY}/apps/desktop/dist-electron/boot.cjs: `72c69bd04c1a486cf3c61a8af3aadebd69fb1605519af0bea183ad406d9096b1`
- ${PREPARATION}/original-desktop-bootstrap.cjs: `9061bbcf2ff95a6bb4f79fde6138b8b019b5941355529fe339b7d63c8028f403`
- ${REPOSITORY}/rust/target/dx/t3-ui/release/macos/T3Ui.app/Contents/MacOS/t3-ui: `737348a1e8261d3a6908cb980c2cc62ec824e502b1d578077b4ee97e05e9819c`
- ${REPOSITORY}/rust/target/dx/t3-ui/release/macos/T3Ui.app/Contents/Info.plist: `7c36e528be3559c60eb4b0d79a933cdbb212df22366c8bf838e656b0fb37da54`
- ${REPOSITORY}/apps/desktop/dist-electron/main.cjs: `503af61b83b9929279784a17440460160b40d40d0193a1dd9e15f141ede9045f`
- ${REPOSITORY}/apps/desktop/dist-electron/preload.cjs: `42df91cba1205d6f821e7338e6898e7e9d7221ea896d66a80d590f7baa74afde`
- ${REPOSITORY}/apps/server/dist/client/index.html: `fd1e25ed24dbe58d55deeb1a57bf656c6530261a346a5a138a38e96b4f353037`
- ${PREPARATION}/run-desktop-lifecycle.mjs: `ae0af944bc87295e2fd8b7b65cc7b9da70f4c2335c9a8bd0a1e3d7e375a9c319`
- ${PREPARATION}/owned-process-lifecycle.mjs: `d884ccf2c5b7cb859cad2c929142ccf98f61dd83089ed3f5d9b20a7b44fcc5bc`

Raw samples, lifecycle markers, WebKit UUIDs, cleanup receipts and input hashes are preserved in t3port-bench-desktop-comparison.json.
