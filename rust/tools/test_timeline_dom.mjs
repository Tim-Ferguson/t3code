// Exercise the actual shipped bridge without starting a browser or server.
import { readFileSync } from "node:fs";
import { strict as assert } from "node:assert";
import vm from "node:vm";
const source = readFileSync(new URL("../crates/ui/src/timeline_dom.js", import.meta.url), "utf8");
async function bridge() {
  const events = new Map(),
    rafs = new Map(),
    emitted = [];
  let next = 0,
    resize,
    mutation,
    receive;
  const listeners = () => ({
    addEventListener(type, fn) {
      events.set(type, fn);
    },
    removeEventListener(type) {
      events.delete(type);
    },
  });
  const row = {
    dataset: { timelineRow: "row", messageId: "message" },
    getBoundingClientRect: () => ({ top: 500, height: 100 }),
  };
  const spacer = { style: { height: "0px" } };
  const node = {
    ...listeners(),
    isConnected: true,
    clientTop: 0,
    clientHeight: 400,
    scrollHeight: 700,
    scrollTop: 0,
    getBoundingClientRect: () => ({ top: 0 }),
    querySelectorAll: () => [row],
    querySelector: () => spacer,
    contains: () => true,
  };
  const document = {
    ...listeners(),
    body: {},
    documentElement: {},
    getElementById: () => node,
    querySelector: () => null,
  };
  const context = vm.createContext({
    timelineId: "timeline",
    document,
    Element: class {},
    getComputedStyle: () => ({ overflowY: "visible" }),
    requestAnimationFrame: (fn) => {
      const id = ++next;
      rafs.set(id, fn);
      return id;
    },
    cancelAnimationFrame: (id) => rafs.delete(id),
    ResizeObserver: class {
      constructor(fn) {
        resize = fn;
      }
      observe() {}
      disconnect() {}
    },
    MutationObserver: class {
      constructor(fn) {
        mutation = fn;
      }
      observe() {}
      disconnect() {}
    },
    dioxus: {
      send: (frame) => emitted.push(frame),
      recv: () => new Promise((resolve) => (receive = resolve)),
    },
  });
  const running = vm.runInContext(`(async()=>{${source}})()`, context);
  const tick = () => {
    const callbacks = [...rafs.values()];
    rafs.clear();
    for (const callback of callbacks) callback();
  };
  tick();
  emitted.length = 0;
  const command = async (value) => {
    receive(value);
    await Promise.resolve();
    await Promise.resolve();
  };
  return {
    events,
    emitted,
    tick,
    command,
    node,
    resize: () => resize(),
    mutation: () => mutation([{ type: "childList", target: node }]),
    stop: async () => {
      await command({ type: "stop" });
      await running;
    },
  };
}
for (const kind of ["layout", "measure"]) {
  const b = await bridge();
  b.events.get("scroll")();
  if (kind === "layout") b.mutation();
  else await b.command({ type: "measure" });
  b.tick();
  assert.equal(b.emitted.at(-1).kind, kind, `scroll→${kind} must preserve geometry-dirty work`);
  await b.stop();
}
{
  const b = await bridge();
  await b.command({ type: "scroll", offset: 300, spacer: 0, intentGeneration: 0 });
  b.tick();
  b.events.get("wheel")({ deltaY: -30, ctrlKey: false, target: null });
  b.tick();
  assert.equal(
    b.node.scrollTop,
    0,
    "a gesture between layout frames cancels the stale imperative write",
  );
  await b.command({ type: "scroll", offset: 400, spacer: 0, intentGeneration: 1 });
  await b.command({ type: "scroll", offset: 200, spacer: 0, intentGeneration: 1 });
  b.tick();
  b.tick();
  assert.equal(
    b.node.scrollTop,
    200,
    "latest owned measurement supersedes an earlier queued write",
  );
  await b.stop();
  assert.equal(b.events.size, 0, "unmount removes listeners");
}
{
  const b = await bridge();
  b.events.get("scroll")();
  b.tick();
  assert.equal(
    b.emitted.at(-1).hasGeometry,
    false,
    "ordinary scroll telemetry reuses measured row geometry",
  );
  assert.equal(b.emitted.at(-1).rowIds, undefined, "scroll events do not resend every history row");
  await b.stop();
}
process.stdout.write(
  "PASS actual timeline DOM bridge: scroll/layout coalescing, scroll/measure coalescing, gesture cancellation, newer write ownership, unmount cleanup\n",
);
