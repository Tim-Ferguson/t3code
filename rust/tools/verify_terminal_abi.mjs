// Exercises the committed WebView ABI router without launching a browser.
// Engine/render behavior is checked separately by verify_terminal_wasm.mjs.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
const code = readFileSync(new URL("../crates/ui/assets/terminal_abi.js", import.meta.url), "utf8");
let replacements = 0;
const routed = code.replace(/import\(\s*args\.base\s*\+\s*(["'])\/t3_terminal\.js\1\s*\)/g, () => {
  replacements++;
  return 'loadModule(args.base + "/t3_terminal.js")';
});
assert.equal(replacements, 1, "The committed terminal ABI module import changed.");
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const run = new AsyncFunction("args", "dioxus", "window", "document", "loadModule", routed);
const deferred = () => {
  let resolve;
  const promise = new Promise((r) => (resolve = r));
  return { promise, resolve };
};
function harness(overrides = {}) {
  const calls = [],
    events = [],
    waiters = [],
    commands = [];
  const ready = deferred();
  const surface = {
    write: (data) => calls.push(["write", data]),
    reset_and_write: (data) => calls.push(["reset", data]),
    set_visible: (value) => calls.push(["visible", value]),
    set_read_only: (value) => calls.push(["readonly", value]),
    resend_size: () => calls.push(["size"]),
    set_font: (family, size) => calls.push(["font", family, size]),
    focus: () => calls.push(["focus"]),
    fit: () => calls.push(["fit"]),
    dispose: () => calls.push(["dispose"]),
    free: () => calls.push(["free"]),
    ...overrides.surface,
  };
  const args = {
    id: "pane",
    base: "/assets/terminal-surface-hashed",
    wasm: "/assets/terminal-surface-hashed/t3_terminal_bg.wasm",
    options: { readOnly: true },
  };
  const window = overrides.window ?? {};
  const module = {
    default: (input) => {
      assert.deepEqual(input, { module_or_path: args.wasm });
      calls.push(["init"]);
    },
    mount_terminal: async (host, options) => {
      assert.equal(host, "host");
      assert.deepEqual(JSON.parse(options), args.options);
      return surface;
    },
    ...overrides.module,
  };
  const dioxus = {
    send: (event) => {
      events.push(event);
      if (event.type === "ready") ready.resolve();
    },
    recv: () =>
      commands.length
        ? Promise.resolve(commands.shift())
        : new Promise((resolve) => waiters.push(resolve)),
  };
  const load =
    overrides.load ??
    (async (path) => {
      assert.equal(path, args.base + "/t3_terminal.js");
      return module;
    });
  const task = run(
    args,
    dioxus,
    window,
    {
      getElementById: () => {
        calls.push(["lookup"]);
        return "host";
      },
    },
    load,
  );
  const send = (command) => (waiters.length ? waiters.shift()(command) : commands.push(command));
  return { args, window, calls, events, task, send, ready, module, surface };
}
const normal = harness();
await normal.ready.promise;
for (const command of [
  { type: "reset", data: "汉🙂", receipt: 1 },
  { type: "append", data: "\x1b[31mred", receipt: 2 },
  { type: "size" },
  { type: "font", family: "Menlo", size: 16 },
  { type: "readonly", readOnly: false },
  { type: "dispose" },
])
  normal.send(command);
await normal.task;
assert.deepEqual(normal.events, [
  { type: "ready" },
  { type: "applied", receipt: 1 },
  { type: "applied", receipt: 2 },
]);
assert.deepEqual(normal.calls.slice(2), [
  ["reset", "汉🙂"],
  ["write", "\x1b[31mred"],
  ["size"],
  ["font", "Menlo", 16],
  ["readonly", false],
  ["dispose"],
  ["free"],
]);
assert.equal(normal.window.__t3RustTerminals.size, 0);

const imported = deferred();
const pre = harness({ load: () => imported.promise });
pre.window.__t3RustTerminals.get("pane").disposed = true;
imported.resolve(pre.module);
await pre.task;
assert.equal(
  pre.calls.some(([op]) => op === "lookup"),
  false,
);
assert.equal(pre.events.length, 0);
assert.equal(pre.window.__t3RustTerminals.size, 0);

const mounting = deferred(),
  started = deferred();
const canceled = harness({
  module: {
    mount_terminal: () => {
      started.resolve();
      return mounting.promise;
    },
  },
});
await started.promise;
canceled.window.__t3RustTerminals.get("pane").disposed = true;
mounting.resolve(canceled.surface);
await canceled.task;
assert.deepEqual(canceled.calls.slice(-2), [["dispose"], ["free"]]);
assert.equal(canceled.events.length, 0);

const failure = harness({
  surface: {
    write: () => {
      throw new Error("renderer terminated");
    },
  },
});
await failure.ready.promise;
failure.send({ type: "append", data: "last update", receipt: 99 });
await failure.task;
assert.equal(
  failure.events.some((event) => event.type === "applied"),
  false,
);
assert.deepEqual(failure.events.at(-1), { type: "error", message: "Error: renderer terminated" });
assert.deepEqual(failure.calls.slice(-2), [["dispose"], ["free"]]);
assert.equal(failure.window.__t3RustTerminals.size, 0);

const delayed = deferred(),
  began = deferred();
const shared = {};
const old = harness({
  window: shared,
  module: {
    mount_terminal: () => {
      began.resolve();
      return delayed.promise;
    },
  },
});
await began.promise;
shared.__t3RustTerminals.get("pane").disposed = true;
const next = harness({ window: shared });
await next.ready.promise;
const owned = shared.__t3RustTerminals.get("pane");
delayed.resolve(old.surface);
await old.task;
assert.equal(shared.__t3RustTerminals.get("pane"), owned);
next.send({ type: "dispose" });
await next.task;
assert.equal(shared.__t3RustTerminals.size, 0);
console.log(
  JSON.stringify({
    abiScenarios: 5,
    explicitWasmUrl: true,
    cancellationCleanup: true,
    receiptFailure: true,
    replacedPaneOwnership: true,
  }),
);
