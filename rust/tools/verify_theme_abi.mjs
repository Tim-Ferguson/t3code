// Executes the committed browser API transport; Rust remains the storage/theme policy owner.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
const code = readFileSync(new URL("../crates/ui/assets/theme_abi.js", import.meta.url), "utf8");
const run = new (Object.getPrototypeOf(async function () {}).constructor)(
  "args",
  "dioxus",
  "window",
  "document",
  "getComputedStyle",
  code,
);
const tick = () => new Promise((resolve) => setImmediate(resolve));
function harness() {
  const listeners = new Map(),
    mediaListeners = new Map(),
    saved = new Map(),
    events = [],
    queue = [],
    waiters = [],
    variables = new Map(),
    metas = [];
  const root = {
    dataset: {},
    style: {
      setProperty: (k, v) => variables.set(k, v),
      removeProperty: (k) => variables.delete(k),
    },
    classList: {
      toggle(k, v) {
        root.dark = v;
      },
    },
  };
  const surface = { backgroundColor: "rgb(250, 250, 250)" },
    body = { backgroundColor: "rgb(255, 255, 255)", style: {} };
  const media = {
    matches: false,
    addEventListener: (k, f) => mediaListeners.set(k, f),
    removeEventListener: (k, f) => {
      assert.equal(mediaListeners.get(k), f);
      mediaListeners.delete(k);
    },
  };
  const window = {
    localStorage: {
      getItem: (k) => saved.get(k) ?? null,
      setItem(k, v) {
        if (k === "fail") throw Error("storage failed");
        saved.set(k, v);
      },
      removeItem: (k) => saved.delete(k),
    },
    matchMedia: () => media,
    addEventListener: (k, f) => listeners.set(k, f),
    removeEventListener: (k, f) => {
      assert.equal(listeners.get(k), f);
      listeners.delete(k);
    },
  };
  const document = {
    documentElement: root,
    body,
    querySelector: () => surface,
    querySelectorAll: () => metas,
    createElement: () => ({
      attributes: {},
      setAttribute(k, v) {
        this.attributes[k] = v;
      },
    }),
    head: { append: (meta) => metas.push(meta) },
  };
  const bridge = {
    send: (e) => events.push(e),
    recv: () =>
      queue.length
        ? Promise.resolve(queue.shift())
        : new Promise((resolve) => waiters.push(resolve)),
  };
  const task = run({ id: "theme" }, bridge, window, document, (node) => node);
  return {
    task,
    events,
    window,
    document,
    root,
    variables,
    saved,
    listeners,
    mediaListeners,
    media,
    metas,
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
  assert.deepEqual(h.events, [{ type: "ready", dark: false }]);
  h.send({ id: 1, type: "set", key: "t3code:theme", value: "ocean" });
  h.send({ id: 2, type: "get", key: "t3code:theme" });
  await tick();
  assert.equal(h.events.at(-1).value, "ocean");
  h.send({ id: 3, type: "remove", key: "t3code:theme" });
  h.send({ id: 4, type: "get", key: "t3code:theme" });
  await tick();
  assert.equal(h.events.at(-1).value, null);
  h.send({ type: "dispose" });
  await h.task;
  assert.equal(h.listeners.size, 0);
  assert.equal(h.mediaListeners.size, 0);
  assert.equal(h.window.__t3RustThemes.size, 0);
  cases++;
}
{
  const h = harness();
  await tick();
  h.media.matches = true;
  h.mediaListeners.get("change")();
  h.listeners.get("storage")({ key: "t3code:theme-halves:v1" });
  assert.deepEqual(h.events.slice(-2), [
    { type: "media", dark: true },
    { type: "storage", key: "t3code:theme-halves:v1" },
  ]);
  const late = h.listeners.get("storage");
  h.window.__t3RustThemes.get("theme").dispose();
  late({ key: null });
  assert.equal(h.events.length, 3);
  h.send({ type: "dispose" });
  await h.task;
  cases++;
}
{
  const h = harness();
  await tick();
  h.send({ id: 1, type: "set", key: "fail", value: "x" });
  h.send({ id: 2, type: "set", key: "okay", value: "retained" });
  await tick();
  assert.match(h.events.find((e) => e.id === 1).error, /storage failed/);
  assert.equal(h.saved.get("okay"), "retained");
  h.send({ type: "dispose" });
  await h.task;
  cases++;
}
{
  const h = harness();
  await tick();
  h.send({
    id: 1,
    type: "apply",
    dark: true,
    paletteId: "ocean",
    variables: { "--app-theme-canvas": "#123456" },
  });
  await tick();
  assert.equal(h.root.dark, true);
  assert.equal(h.root.dataset.themeId, "ocean");
  assert.equal(h.variables.get("--app-theme-canvas"), "#123456");
  assert.deepEqual(h.events.at(-1).value, {
    surface: "rgb(250, 250, 250)",
    body: "rgb(255, 255, 255)",
  });
  h.send({ id: 2, type: "chrome", color: "#123456" });
  await tick();
  assert.equal(h.metas[0].attributes.content, "#123456");
  h.send({
    id: 3,
    type: "apply",
    dark: false,
    paletteId: null,
    variables: { "--app-theme-canvas": null },
  });
  h.send({ id: 4, type: "chrome", color: "rgb(250, 250, 250)" });
  await tick();
  assert.equal(h.root.dataset.themeId, undefined);
  assert.equal(h.variables.size, 0);
  assert.equal(h.metas[0].attributes.content, "rgb(250, 250, 250)");
  assert.equal(h.document.body.style.backgroundColor, "rgb(250, 250, 250)");
  h.send({ type: "dispose" });
  await h.task;
  cases++;
}
console.log(JSON.stringify({ cases, actualAbi: true }));
