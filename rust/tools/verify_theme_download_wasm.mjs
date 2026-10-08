// Actual production WASM HTTP adapter against deterministic Web Streams.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url);
const wasm = require("../target/theme-download-oracle-node/t3_theme_download_wasm_oracle.js");
const root = "https://open-vsx.org/api/demo/theme/1.0.0/file";
const detail = {
  namespace: "demo",
  name: "theme",
  displayName: "Demo Theme",
  version: "1.0.0",
  license: "MIT",
  description: "A nice theme",
  downloadCount: 123456,
  repository: "https://github.com/demo/theme",
  files: {
    icon: root + "/icon.png",
    manifest: root + "/package.json",
    sha256: root + "/theme.sha256",
    download: root + "/theme.vsix",
  },
};
const NativeReader = globalThis.ReadableStreamDefaultReader;
let bodyReads = 0;
const tracked = new WeakSet();
globalThis.ReadableStreamDefaultReader = class extends NativeReader {
  constructor(stream) {
    super(stream);
    if (tracked.has(stream)) bodyReads++;
  }
};
const tick = () => new Promise((resolve) => setImmediate(resolve));
let sequence = 0;
for (const line of readFileSync(
  new URL("../crates/ui/tests/fixtures/openvsx-network.jsonl", import.meta.url),
  "utf8",
)
  .trimEnd()
  .split("\n")) {
  const { input, expected } = JSON.parse(line);
  bodyReads = 0;
  globalThis.fetch = async (url) => {
    if (String(url).includes("/-/search"))
      return new Response(JSON.stringify({ extensions: [{ namespace: "demo", name: "theme" }] }));
    if (String(url) === "https://open-vsx.org/api/demo/theme")
      return new Response(JSON.stringify(detail));
    if (String(url).endsWith("/theme.vsix"))
      return new Response(null, {
        status: input.packageOk ? 200 : 404,
        headers: input.packageLength === null ? {} : { "content-length": input.packageLength },
      });
    const stream = new ReadableStream({
      start(controller) {
        if (input.body === "failure") controller.error(Error("body failed"));
        else {
          controller.enqueue(
            new TextEncoder().encode(
              '{"license":"MIT","contributes":{"themes":[{"path":"./theme.json"}]}}',
            ),
          );
          controller.close();
        }
      },
    });
    tracked.add(stream);
    return new Response(stream, {
      status: input.manifestOk ? 200 : 404,
      headers: input.body === "oversized" ? { "content-length": "262145" } : {},
    });
  };
  const actual = JSON.parse(await wasm.search());
  assert.deepEqual(
    { ...actual, bodyReads },
    expected,
    `source headers/body sequence ${sequence++}`,
  );
  await tick();
}
let caps = 0;
for (const [limit, chunks, declared, expected] of [
  [4, [2, 2], null, { bytes: 4 }],
  [4, [2, 3], null, { error: "body too large" }],
  [4, [5], "5", { error: "body too large" }],
  [4, [2], "Infinity", { error: "body too large" }],
  [20 * 1024 * 1024, [20 * 1024 * 1024 + 1], null, { error: "body too large" }],
]) {
  let canceled = 0;
  globalThis.fetch = async () =>
    new Response(
      new ReadableStream({
        start(controller) {
          for (const n of chunks) controller.enqueue(new Uint8Array(n));
        },
        cancel() {
          canceled++;
        },
      }),
      { headers: declared === null ? {} : { "content-length": declared } },
    );
  // Exact-size successes must terminate while overflow witnesses remain open.
  if (expected.bytes !== undefined)
    globalThis.fetch = async () =>
      new Response(
        new ReadableStream({
          start(c) {
            for (const n of chunks) c.enqueue(new Uint8Array(n));
            c.close();
          },
        }),
      );
  assert.deepEqual(JSON.parse(await wasm.capped("https://open-vsx.org/test", limit)), expected);
  await tick();
  if (expected.error && declared === null) assert.equal(canceled, 1);
  caps++;
}
const unhandled = [];
const listener = (reason) => unhandled.push(String(reason));
process.on("unhandledRejection", listener);
let canceled = 0,
  aborted = 0;
globalThis.fetch = async (_url, init) => {
  init.signal.addEventListener("abort", () => aborted++);
  return new Response(
    new ReadableStream({
      pull() {
        return new Promise(() => {});
      },
      cancel() {
        canceled++;
        return Promise.reject(Error("cancel rejected witness"));
      },
    }),
  );
};
await wasm.cancel("https://open-vsx.org/pending");
await tick();
await tick();
assert.equal(canceled, 1);
assert.equal(aborted, 1);
assert.deepEqual(unhandled, []);
let timeoutCanceled = 0,
  timeoutAborted = 0;
globalThis.fetch = async (_url, init) => {
  init.signal.addEventListener("abort", () => timeoutAborted++);
  return new Response(
    new ReadableStream({
      pull() {
        return new Promise(() => {});
      },
      cancel() {
        timeoutCanceled++;
      },
    }),
  );
};
assert.deepEqual(JSON.parse(await wasm.search()), {
  value: null,
  error: "Open VSX took too long to respond.",
});
await tick();
assert.equal(timeoutCanceled, 1);
assert.equal(timeoutAborted, 1);
assert.deepEqual(unhandled, []);
process.off("unhandledRejection", listener);
globalThis.ReadableStreamDefaultReader = NativeReader;
console.log(
  JSON.stringify({
    sourceHttpSequences: sequence,
    streamingCaps: caps,
    dropCancelsPendingBody: true,
    cancelRejectionObserved: true,
    deadlineIncludesPendingBody: true,
  }),
);
