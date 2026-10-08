// Actual committed ABI router, with deterministic module/permission lifetimes.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
const text = readFileSync(
  new URL("../crates/ui/assets/appearance_abi.js", import.meta.url),
  "utf8",
);
let replacements = 0;
const code = text.replace(/import\(\s*args\.base\s*\+\s*(["'])\/t3_terminal\.js\1\s*\)/g, () => {
  replacements++;
  return 'loadModule(args.base + "/t3_terminal.js")';
});
assert.equal(replacements, 1, "Appearance ABI import changed.");
const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const run = new AsyncFunction("args", "dioxus", "window", "loadModule", code);
const deferred = () => {
  let resolve;
  const promise = new Promise((r) => (resolve = r));
  return { resolve, promise };
};
const tick = () => new Promise((resolve) => setImmediate(resolve));
function harness(overrides = {}) {
  const args = {
      id: "appearance",
      base: "/assets/hashed-terminal",
      wasm: "/assets/hashed-terminal/t3_terminal_bg.wasm",
    },
    events = [],
    calls = [],
    queue = [],
    waiters = [],
    window = {};
  const module = {
    default(input) {
      assert.deepEqual(input, { module_or_path: args.wasm });
      calls.push(["init"]);
    },
    appearance_default_fonts: () =>
      JSON.stringify({ sans: "System default", code: "System monospace" }),
    appearance_apply_fonts: (raw) => calls.push(["apply", JSON.parse(raw)]),
    appearance_probe_font: (family) =>
      JSON.stringify({ available: family === "Menlo", monospace: true }),
    appearance_font_permission: async () => "prompt",
    appearance_query_fonts: async () => JSON.stringify({ status: "granted", families: ["Menlo"] }),
    ...overrides.module,
  };
  const bridge = {
    send: (event) => events.push(event),
    recv: () =>
      queue.length
        ? Promise.resolve(queue.shift())
        : new Promise((resolve) => waiters.push(resolve)),
  };
  const task = run(
    args,
    bridge,
    window,
    overrides.load ??
      (async (path) => {
        assert.equal(path, args.base + "/t3_terminal.js");
        return module;
      }),
  );
  return {
    events,
    calls,
    window,
    module,
    task,
    send(command) {
      const waiter = waiters.shift();
      if (waiter) waiter(command);
      else queue.push(command);
    },
  };
}
let cases = 0;
{
  const h = harness();
  await tick();
  assert.equal(h.events[0].type, "ready");
  h.send({ type: "apply", settings: { fontFamilyCode: "Menlo" } });
  h.send({ type: "probe", id: 1, family: "Menlo" });
  await tick();
  assert.deepEqual(h.calls.at(-1), ["apply", { fontFamilyCode: "Menlo" }]);
  assert.deepEqual(h.events.at(-1), {
    type: "response",
    id: 1,
    value: { available: true, monospace: true },
  });
  h.send({ type: "dispose" });
  await h.task;
  assert.equal(h.window.__t3RustAppearances.size, 0);
  cases++;
}
{
  const permission = deferred(),
    h = harness({ module: { appearance_font_permission: () => permission.promise } });
  await tick();
  h.send({ type: "permission", id: 1 });
  h.send({ type: "probe", id: 2, family: "Menlo" });
  h.send({ type: "apply", settings: { fontSizeCode: 16 } });
  await tick();
  assert.equal(h.events.at(-1).id, 2);
  assert.equal(h.calls.at(-1)[0], "apply");
  permission.resolve("granted");
  await tick();
  assert.equal(h.events.at(-1).id, 1);
  h.send({ type: "dispose" });
  await h.task;
  cases++;
}
{
  const query = deferred(),
    h = harness({ module: { appearance_query_fonts: () => query.promise } });
  await tick();
  h.send({ type: "enumerate", id: 1 });
  await tick();
  h.send({ type: "dispose" });
  await h.task;
  query.resolve(JSON.stringify({ status: "granted", families: ["Menlo"] }));
  await tick();
  assert.deepEqual(
    h.events.map((event) => event.type),
    ["ready"],
  );
  cases++;
}
{
  const load = deferred(),
    h = harness({ load: () => load.promise });
  h.window.__t3RustAppearances.get("appearance").disposed = true;
  load.resolve(h.module);
  await h.task;
  assert.equal(h.events.length, 0);
  assert.equal(h.window.__t3RustAppearances.size, 0);
  cases++;
}
{
  const h = harness({
    module: {
      appearance_probe_font() {
        throw Error("measurement failed");
      },
    },
  });
  await tick();
  h.send({ type: "probe", id: 1, family: "bad" });
  await tick();
  assert.match(h.events.at(-1).error, /measurement failed/);
  h.send({ type: "apply", settings: { fontSizeCode: 16 } });
  await tick();
  assert.equal(h.calls.at(-1)[0], "apply");
  h.send({ type: "dispose" });
  await h.task;
  cases++;
}
{
  const h = harness({
    load: async () => {
      throw Error("missing Rust module");
    },
  });
  await h.task;
  assert.equal(h.events[0].type, "error");
  assert.equal(h.window.__t3RustAppearances.size, 0);
  cases++;
}
console.log(JSON.stringify({ cases, actualAbi: true }));
